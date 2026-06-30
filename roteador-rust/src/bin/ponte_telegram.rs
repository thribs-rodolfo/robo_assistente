//! Ponte Telegram em Rust — servidor de webhooks (substitui o `servidor.py`).
//!
//! Escuta em `127.0.0.1:18800`. O nginx termina o HTTPS e repassa para cá. Rotas:
//!   - `POST /ponte-telegram/<nome_bot>` — recebe um update do Telegram.
//!   - `GET  /ponte-telegram/saude`      — healthcheck (responde "ok").
//!
//! Fluxo de um POST (espelha o `servidor.py`, agora tipado e sem exceções):
//!   1. acha o bot pelo nome do caminho; 404 se não existir;
//!   2. valida o `secret` do webhook (cabeçalho `X-Telegram-Bot-Api-Secret-Token`); 403 se não bater;
//!   3. responde 200 IMEDIATAMENTE ao Telegram (boa prática: não segurar a conexão);
//!   4. processa o update em seguida (rotear -> responder no chat).
//!
//! Concorrência: uma thread da stdlib por conexão (sem dependências). Cada update é
//! independente, então isolá-los em threads é simples e seguro.

use std::net::{TcpListener, TcpStream};
use std::thread;

use roteador::config::{carregar_de_arquivo, Config, CAMINHO_PADRAO};
use roteador::ponte::{self, ConfigBot};
use roteador::servidor_http::{ler_requisicao, Requisicao, Resposta};

/// Endereço de escuta padrão. Só localhost: o nginx é quem fala com o mundo (e faz o TLS).
/// Pode ser sobrescrito pela variável de ambiente `PONTE_ENDERECO` (útil para testes locais
/// em uma porta descartável, sem conflitar com o serviço em produção na 18800).
const ENDERECO_PADRAO: &str = "127.0.0.1:18800";

/// Cabeçalho onde o Telegram envia o secret do webhook.
const CABECALHO_SECRET: &str = "X-Telegram-Bot-Api-Secret-Token";

fn main() {
    let endereco = std::env::var("PONTE_ENDERECO").unwrap_or_else(|_| ENDERECO_PADRAO.to_string());
    ponte::registrar(&format!("ponte-telegram (Rust) iniciando em {endereco}"));

    let escuta = match TcpListener::bind(&endereco) {
        Ok(l) => l,
        Err(erro) => {
            // Sem o socket não há ponte: registra e encerra com código de erro.
            ponte::registrar(&format!(
                "FATAL: não consegui escutar em {endereco}: {erro}"
            ));
            eprintln!("não consegui escutar em {endereco}: {erro}");
            std::process::exit(1);
        }
    };

    for conexao in escuta.incoming() {
        match conexao {
            Ok(socket) => {
                // Uma thread por conexão. Se a thread morrer, a ponte segue de pé.
                thread::spawn(move || atender(socket));
            }
            Err(erro) => ponte::registrar(&format!("conexão recusada: {erro}")),
        }
    }
}

/// Atende uma conexão: lê a requisição, decide a resposta e (se for update) processa.
fn atender(mut socket: TcpStream) {
    // Lemos a requisição de um clone do socket para poder escrever no original depois.
    let socket_leitura = match socket.try_clone() {
        Ok(s) => s,
        Err(erro) => {
            ponte::registrar(&format!("não clonei o socket: {erro}"));
            return;
        }
    };

    let requisicao = match ler_requisicao(socket_leitura) {
        Ok(r) => r,
        Err(erro) => {
            ponte::registrar(&format!("requisição inválida: {erro}"));
            let _ = Resposta::texto(400, "bad request").escrever_em(&mut socket);
            return;
        }
    };

    let (resposta, trabalho) = rotear_requisicao(&requisicao);

    // 1) Responde ao Telegram primeiro (200 rápido, ou o erro adequado).
    if let Err(erro) = resposta.escrever_em(&mut socket) {
        ponte::registrar(&format!("falha ao responder no socket: {erro}"));
        return;
    }

    // 2) Só então faz o trabalho pesado (rotear + sendMessage), com a conexão já liberada.
    if let Some(processar) = trabalho {
        processar();
    }
}

/// Decide a resposta HTTP para uma requisição e, se for um update válido, devolve também
/// o trabalho a executar DEPOIS de responder (uma closure). Separa "o que responder" de
/// "o que fazer", o que deixa o roteamento de rotas testável sem rede.
fn rotear_requisicao(req: &Requisicao) -> (Resposta, Option<Box<dyn FnOnce() + Send>>) {
    // Healthcheck.
    if req.metodo == "GET" && req.caminho == "/ponte-telegram/saude" {
        return (Resposta::texto(200, "ok"), None);
    }

    // A partir daqui só tratamos POST /ponte-telegram/<nome_bot>.
    let nome_bot = match nome_bot_do_caminho(&req.caminho) {
        Some(n) if req.metodo == "POST" => n,
        _ => return (Resposta::texto(404, "not found"), None),
    };

    // Carrega a config dos bots a cada requisição (permite editar secrets sem reiniciar).
    let bots = match ponte::carregar_config(ponte::CAMINHO_CONFIG_PADRAO) {
        Ok(b) => b,
        Err(erro) => {
            ponte::registrar(&format!("config dos bots ilegível: {erro}"));
            return (Resposta::texto(500, "config error"), None);
        }
    };

    let bot = match ponte::achar_bot(&bots, nome_bot) {
        Some(b) => b.clone(),
        None => return (Resposta::texto(404, "bot desconhecido"), None),
    };

    // Valida o secret do webhook.
    let recebido = req.cabecalho(CABECALHO_SECRET).unwrap_or("");
    if recebido != bot.secret {
        ponte::registrar(&format!("[{}] secret inválido", bot.nome));
        return (Resposta::texto(403, "forbidden"), None);
    }

    // Carrega a config do roteador (provedores). Se falhar, ainda respondemos 200 ao
    // Telegram (não queremos reentregas), mas registramos e não processamos.
    let config_roteador = match carregar_de_arquivo(CAMINHO_PADRAO) {
        Ok(c) => c,
        Err(erro) => {
            ponte::registrar(&format!(
                "[{}] config do roteador ilegível: {erro}",
                bot.nome
            ));
            return (resposta_ok_telegram(), None);
        }
    };

    // Tudo certo: 200 imediato + trabalho de processamento adiado.
    let corpo_update = req.corpo.clone();
    let trabalho = mover_processamento(bot, corpo_update, config_roteador);
    (resposta_ok_telegram(), Some(trabalho))
}

/// Empacota o processamento do update em uma closure para rodar após responder o Telegram.
fn mover_processamento(
    bot: ConfigBot,
    corpo_update: String,
    config_roteador: Config,
) -> Box<dyn FnOnce() + Send> {
    Box::new(move || ponte::processar(&bot, &corpo_update, &config_roteador))
}

/// Resposta 200 padrão que o Telegram espera de um webhook.
fn resposta_ok_telegram() -> Resposta {
    Resposta::json(200, "{\"ok\":true}")
}

/// Extrai `<nome_bot>` de um caminho `/ponte-telegram/<nome_bot>`. `None` se não casar.
fn nome_bot_do_caminho(caminho: &str) -> Option<&str> {
    let partes: Vec<&str> = caminho.trim_matches('/').split('/').collect();
    if partes.len() == 2 && partes[0] == "ponte-telegram" && !partes[1].is_empty() {
        Some(partes[1])
    } else {
        None
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn extrai_nome_do_bot() {
        assert_eq!(
            nome_bot_do_caminho("/ponte-telegram/ronaldo"),
            Some("ronaldo")
        );
        assert_eq!(nome_bot_do_caminho("/ponte-telegram/teste/"), Some("teste"));
        assert_eq!(nome_bot_do_caminho("/outra-coisa/x"), None);
        assert_eq!(nome_bot_do_caminho("/ponte-telegram/"), None);
        assert_eq!(nome_bot_do_caminho("/ponte-telegram"), None);
    }

    #[test]
    fn healthcheck_responde_ok_sem_trabalho() {
        let req = Requisicao {
            metodo: "GET".into(),
            caminho: "/ponte-telegram/saude".into(),
            cabecalhos: vec![],
            corpo: String::new(),
        };
        let (resposta, trabalho) = rotear_requisicao(&req);
        assert_eq!(resposta.status, 200);
        assert_eq!(resposta.corpo, "ok");
        assert!(trabalho.is_none());
    }

    #[test]
    fn rota_desherecida_da_404() {
        let req = Requisicao {
            metodo: "GET".into(),
            caminho: "/nada".into(),
            cabecalhos: vec![],
            corpo: String::new(),
        };
        let (resposta, _) = rotear_requisicao(&req);
        assert_eq!(resposta.status, 404);
    }
}
