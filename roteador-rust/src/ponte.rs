//! Ponte Telegram: a cola entre o webhook do Telegram e o [`crate::rotear`].
//!
//! Substitui o `servidor.py`. Para cada bot configurado, a ponte:
//! 1. valida o `secret` do webhook (o Telegram manda no cabeçalho);
//! 2. confere se quem falou está no `allow_from` (lista branca — seguro por padrão);
//! 3. roteia o texto pelos provedores (cadeia de fallback do roteador);
//! 4. responde no chat via API do Telegram (`sendMessage`, sobre HTTPS via `curl`).
//!
//! A config (com TOKENS) mora FORA do repositório, em `/root/.secrets/ponte-telegram.json`.
//! Aqui só lemos, validamos e usamos. Erros são valores tipados; nada é engolido em silêncio.

use std::time::Duration;

use crate::erro::FalhaProvedor;
use crate::https;
use crate::json::{self, Valor};
use crate::prompt::Contexto;
use crate::{rotear, Config};

/// Caminho padrão da config dos bots (fora do repositório, com os tokens).
pub const CAMINHO_CONFIG_PADRAO: &str = "/root/.secrets/ponte-telegram.json";

/// Arquivo de log da ponte (continuidade com o `servidor.py`).
pub const ARQUIVO_LOG: &str = "/var/log/ponte-telegram.log";

/// Tempo limite para a chamada `sendMessage` ao Telegram.
const TIMEOUT_TELEGRAM: Duration = Duration::from_secs(20);

/// Configuração de um bot do Telegram, já resolvida (allowFrom expandido).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigBot {
    /// Nome lógico do bot (último segmento do caminho do webhook). Ex.: "ronaldo".
    pub nome: String,
    /// Token da API do Telegram (segredo — vem da config fora do repo).
    pub token: String,
    /// Secret do webhook: o Telegram o envia no cabeçalho a cada update.
    pub secret: String,
    /// IDs autorizados a falar com o bot (lista branca). Vazia = ninguém (seguro por padrão).
    pub permitidos: Vec<String>,
    /// Instrução de sistema opcional para o roteador (personalidade/contexto do bot).
    pub sistema: Option<String>,
}

/// Erro ao carregar/usar a config da ponte. Valor tipado, nunca pânico.
#[derive(Debug, Clone, PartialEq)]
pub enum ErroPonte {
    /// Não foi possível ler ou parsear o arquivo de config.
    Config(String),
}

impl std::fmt::Display for ErroPonte {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErroPonte::Config(motivo) => write!(f, "config da ponte inválida: {motivo}"),
        }
    }
}

impl std::error::Error for ErroPonte {}

/// Lê e parseia a config dos bots a partir do caminho padrão.
pub fn carregar_config(caminho: &str) -> Result<Vec<ConfigBot>, ErroPonte> {
    let conteudo = std::fs::read_to_string(caminho)
        .map_err(|e| ErroPonte::Config(format!("não li '{caminho}': {e}")))?;
    interpretar_config(&conteudo)
}

/// Parseia a config dos bots a partir do texto JSON (separado da E/S para ser testável).
///
/// Formato:
/// ```json
/// { "bots": { "<nome>": { "token": "...", "secret": "...",
///                         "allow_from": [ids]  OU  "allow_from_arquivo": "/caminho.json",
///                         "sistema": "instrução opcional" } } }
/// ```
pub fn interpretar_config(texto_json: &str) -> Result<Vec<ConfigBot>, ErroPonte> {
    let raiz = json::parsear(texto_json).map_err(|e| ErroPonte::Config(e.to_string()))?;
    let bots_objeto = match raiz.obter("bots") {
        Some(Valor::Objeto(pares)) => pares,
        _ => return Err(ErroPonte::Config("falta o objeto 'bots'".into())),
    };

    let mut bots = Vec::new();
    for (nome, valor) in bots_objeto {
        let token = texto_obrigatorio(valor, "token")
            .ok_or_else(|| ErroPonte::Config(format!("bot '{nome}' sem 'token'")))?;
        let secret = texto_obrigatorio(valor, "secret").unwrap_or_default();
        let sistema = valor
            .obter("sistema")
            .and_then(Valor::como_texto)
            .map(str::to_string);
        let permitidos = lista_permitidos(valor);
        bots.push(ConfigBot {
            nome: nome.to_string(),
            token,
            secret,
            permitidos,
            sistema,
        });
    }
    Ok(bots)
}

/// Acha um bot pelo nome na lista carregada.
pub fn achar_bot<'a>(bots: &'a [ConfigBot], nome: &str) -> Option<&'a ConfigBot> {
    bots.iter().find(|b| b.nome == nome)
}

/// Resolve a lista de IDs autorizados de um bot: ou `allow_from` (lista inline),
/// ou `allow_from_arquivo` (aponta para um JSON com a chave `allowFrom`).
/// Lista vazia = ninguém autorizado (escolha segura por padrão).
fn lista_permitidos(valor_bot: &Valor) -> Vec<String> {
    // 1) lista inline tem prioridade.
    if let Some(lista) = valor_bot.obter("allow_from").and_then(Valor::como_lista) {
        return lista.iter().filter_map(id_como_texto).collect();
    }
    // 2) senão, lê o arquivo externo (ex.: allowFrom compartilhado com outra ferramenta).
    if let Some(caminho) = valor_bot
        .obter("allow_from_arquivo")
        .and_then(Valor::como_texto)
    {
        match std::fs::read_to_string(caminho) {
            Ok(conteudo) => return ler_allow_from_arquivo(&conteudo),
            Err(e) => {
                // Sem erro silencioso: registra e segue com lista vazia (= ninguém).
                registrar(&format!("allow_from_arquivo '{caminho}' ilegível: {e}"));
                return Vec::new();
            }
        }
    }
    Vec::new()
}

/// Extrai a chave `allowFrom` de um arquivo JSON externo, como lista de IDs em texto.
fn ler_allow_from_arquivo(conteudo: &str) -> Vec<String> {
    match json::parsear(conteudo) {
        Ok(valor) => valor
            .obter("allowFrom")
            .and_then(Valor::como_lista)
            .map(|lista| lista.iter().filter_map(id_como_texto).collect())
            .unwrap_or_default(),
        Err(e) => {
            registrar(&format!("allow_from_arquivo com JSON inválido: {e}"));
            Vec::new()
        }
    }
}

/// Converte um valor de ID (número OU texto) para texto, para comparar de forma uniforme.
/// O Telegram usa IDs numéricos, mas aceitamos texto também por robustez.
fn id_como_texto(valor: &Valor) -> Option<String> {
    match valor {
        Valor::Numero(n) => Some(format!("{}", *n as i64)),
        Valor::Texto(t) => Some(t.clone()),
        _ => None,
    }
}

/// Lê um campo de texto obrigatório de um objeto de bot.
fn texto_obrigatorio(valor: &Valor, chave: &str) -> Option<String> {
    valor
        .obter(chave)
        .and_then(Valor::como_texto)
        .map(str::to_string)
}

/// Uma mensagem recebida do Telegram, já extraída do update bruto.
#[derive(Debug, Clone, PartialEq)]
pub struct MensagemRecebida {
    /// ID de quem enviou (para checar contra o allowFrom).
    pub remetente: String,
    /// ID do chat (para responder no lugar certo).
    pub chat: i64,
    /// Texto da mensagem.
    pub texto: String,
}

/// Extrai a mensagem de um update do Telegram. Devolve `None` se o update não tiver
/// mensagem de texto (ex.: edições sem texto, eventos de serviço) — aí não há o que fazer.
///
/// Aceita tanto `message` quanto `edited_message` (mensagem editada conta como nova).
pub fn extrair_mensagem(corpo_json: &str) -> Option<MensagemRecebida> {
    let raiz = json::parsear(corpo_json).ok()?;
    let msg = raiz
        .obter("message")
        .or_else(|| raiz.obter("edited_message"))?;

    let remetente = msg
        .obter("from")
        .and_then(|f| f.obter("id"))
        .and_then(Valor::como_numero)
        .map(|n| format!("{}", n as i64))?;
    let chat = msg
        .obter("chat")
        .and_then(|c| c.obter("id"))
        .and_then(Valor::como_numero)
        .map(|n| n as i64)?;
    let texto = msg
        .obter("text")
        .and_then(Valor::como_texto)
        .unwrap_or("")
        .to_string();

    Some(MensagemRecebida {
        remetente,
        chat,
        texto,
    })
}

/// Decide se um remetente está autorizado a falar com o bot.
pub fn autorizado(bot: &ConfigBot, remetente: &str) -> bool {
    bot.permitidos.iter().any(|id| id == remetente)
}

/// Monta o corpo JSON da chamada `sendMessage` do Telegram.
/// Separado para ser testável sem rede.
pub fn corpo_send_message(chat: i64, texto: &str) -> String {
    Valor::Objeto(vec![
        ("chat_id".to_string(), Valor::Numero(chat as f64)),
        ("text".to_string(), Valor::Texto(texto.to_string())),
    ])
    .para_texto()
}

/// Envia uma mensagem de texto via API do Telegram (`sendMessage`, HTTPS).
pub fn enviar_mensagem(token: &str, chat: i64, texto: &str) -> Result<(), FalhaProvedor> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let corpo = corpo_send_message(chat, texto);
    let resposta = https::post_json(&url, &corpo, &[], TIMEOUT_TELEGRAM)?;
    if (200..300).contains(&resposta.status) {
        Ok(())
    } else {
        Err(FalhaProvedor::Http {
            status: resposta.status,
            corpo: resposta.corpo,
        })
    }
}

/// Processa um update completo de um bot: extrai a mensagem, checa autorização,
/// roteia pelos provedores e responde no Telegram. Toda decisão é logada.
///
/// `config_roteador` é passado por parâmetro (injeção de dependência) para testabilidade
/// e para não reler o arquivo a cada update.
pub fn processar(bot: &ConfigBot, corpo_update: &str, config_roteador: &Config) {
    let mensagem = match extrair_mensagem(corpo_update) {
        Some(m) => m,
        None => {
            registrar(&format!(
                "[{}] update sem mensagem de texto — ignorado",
                bot.nome
            ));
            return;
        }
    };

    if !autorizado(bot, &mensagem.remetente) {
        registrar(&format!(
            "[{}] IGNORADO de {} (fora do allow_from)",
            bot.nome, mensagem.remetente
        ));
        return;
    }

    let resumo: String = mensagem.texto.chars().take(120).collect();
    registrar(&format!(
        "[{}] msg de {}: {:?}",
        bot.nome, mensagem.remetente, resumo
    ));

    // Roteia pela cadeia de fallback. O Ollama local na cauda garante que quase sempre
    // há resposta; só se TUDO falhar mandamos um aviso curto (sem deixar o usuário no vácuo).
    let contexto = Contexto {
        sistema: Some(bot.sistema.clone().unwrap_or_else(sistema_padrao)),
        historico: Vec::new(),
    };
    let resposta = match rotear(&mensagem.texto, &contexto, config_roteador) {
        Ok(roteada) => {
            registrar(&format!(
                "[{}] respondido pelo provedor '{}'",
                bot.nome, roteada.provedor
            ));
            roteada.texto
        }
        Err(erro) => {
            registrar(&format!("[{}] roteador falhou: {erro}", bot.nome));
            "Desculpe, estou sem conseguir pensar agora (todos os provedores falharam). \
             Tente de novo em instantes."
                .to_string()
        }
    };

    if let Err(erro) = enviar_mensagem(&bot.token, mensagem.chat, &resposta) {
        registrar(&format!(
            "[{}] falha ao responder no Telegram: {erro}",
            bot.nome
        ));
    }
}

/// Instrução de sistema padrão, usada quando o bot não define uma `sistema` própria.
fn sistema_padrao() -> String {
    "Você é um assistente prestativo respondendo no Telegram. \
     Seja conciso e em português do Brasil."
        .to_string()
}

/// Registra uma linha no log da ponte (best-effort, reaproveitando a telemetria).
pub fn registrar(mensagem: &str) {
    crate::telemetria::registrar_em(ARQUIVO_LOG, mensagem);
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn interpreta_config_com_dois_bots() {
        let bruto = r#"{
            "bots": {
                "teste":   {"token": "T1", "secret": "S1", "allow_from": [111, 222]},
                "ronaldo": {"token": "T2", "secret": "S2", "sistema": "Você é o Ronaldo."}
            }
        }"#;
        let bots = interpretar_config(bruto).unwrap();
        let teste = achar_bot(&bots, "teste").unwrap();
        assert_eq!(teste.token, "T1");
        assert_eq!(teste.permitidos, vec!["111", "222"]);
        let ronaldo = achar_bot(&bots, "ronaldo").unwrap();
        assert_eq!(ronaldo.sistema.as_deref(), Some("Você é o Ronaldo."));
        assert!(ronaldo.permitidos.is_empty()); // sem allow_from = ninguém
    }

    #[test]
    fn rejeita_config_sem_token() {
        let bruto = r#"{"bots":{"x":{"secret":"s"}}}"#;
        assert!(interpretar_config(bruto).is_err());
    }

    #[test]
    fn extrai_mensagem_de_update_normal() {
        let update = r#"{"update_id":1,"message":{"message_id":9,
            "from":{"id":12345,"first_name":"Thiago"},
            "chat":{"id":12345,"type":"private"},"text":"oi"}}"#;
        let m = extrair_mensagem(update).unwrap();
        assert_eq!(m.remetente, "12345");
        assert_eq!(m.chat, 12345);
        assert_eq!(m.texto, "oi");
    }

    #[test]
    fn extrai_mensagem_editada() {
        let update = r#"{"edited_message":{"from":{"id":7},"chat":{"id":7},"text":"editei"}}"#;
        let m = extrair_mensagem(update).unwrap();
        assert_eq!(m.remetente, "7");
        assert_eq!(m.texto, "editei");
    }

    #[test]
    fn update_sem_mensagem_da_none() {
        assert_eq!(extrair_mensagem(r#"{"update_id":1}"#), None);
    }

    #[test]
    fn autorizacao_respeita_lista_branca() {
        let bot = ConfigBot {
            nome: "b".into(),
            token: "t".into(),
            secret: "s".into(),
            permitidos: vec!["100".into()],
            sistema: None,
        };
        assert!(autorizado(&bot, "100"));
        assert!(!autorizado(&bot, "999"));
    }

    #[test]
    fn corpo_send_message_escapa_texto() {
        let corpo = corpo_send_message(42, "linha1\nlinha2 \"aspas\"");
        // O texto precisa sair escapado e o chat_id como inteiro (sem .0).
        assert!(corpo.contains("\"chat_id\":42"));
        assert!(corpo.contains("\\n"));
        assert!(corpo.contains("\\\"aspas\\\""));
    }

    #[test]
    fn allow_from_inline_aceita_numero_e_texto() {
        let valor = json::parsear(r#"{"allow_from":[111,"222"]}"#).unwrap();
        assert_eq!(lista_permitidos(&valor), vec!["111", "222"]);
    }
}
