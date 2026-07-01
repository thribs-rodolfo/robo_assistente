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

/// Quantas vezes RE-enviar uma resposta ao Telegram além da tentativa original, quando a
/// entrega falha de forma transitória (o servidor REJEITOU: 429/5xx/408). Motivo: a resposta
/// já foi GERADA — às vezes com uma chamada cara ao Claude — e perdê-la para um blip do
/// Telegram deixaria o usuário mudo com uma resposta boa na mão. Estende a promessa central
/// do projeto ("o robô nunca fica mudo") da GERAÇÃO para a ENTREGA.
const MAX_RETENTATIVAS_ENVIO: u32 = 2;

/// Base do backoff exponencial entre reenvios (250→500→1000ms...). Só usado quando o Telegram
/// NÃO diz explicitamente quanto esperar (ver `retry_after` no caso 429).
const ESPERA_BASE_ENVIO_MS: u64 = 500;

/// Teto de espera entre reenvios. Vale sobretudo para o `retry_after` de um 429, que o Telegram
/// pode pedir grande: estamos numa thread por mensagem, mas travá-la muitos segundos não ajuda.
const TETO_ESPERA_ENVIO_MS: u64 = 8_000;

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

/// Limite de caracteres por mensagem no Telegram. O limite oficial é 4096; usamos uma
/// margem (4000) por segurança, já que o Telegram conta em unidades UTF-16 e não em chars.
const LIMITE_TELEGRAM: usize = 4000;

/// Envia uma resposta via Telegram, quebrando em várias mensagens se passar do limite.
///
/// Sem a quebra, uma resposta longa (Claude às vezes responde muito) faria o `sendMessage`
/// devolver HTTP 400 e o usuário ficaria SEM resposta. Aqui dividimos em partes e mandamos
/// cada uma; se qualquer parte falhar, devolvemos o erro (sem engolir).
pub fn enviar_mensagem(token: &str, chat: i64, texto: &str) -> Result<(), FalhaProvedor> {
    let partes = dividir_resposta(texto, LIMITE_TELEGRAM);
    for parte in partes {
        enviar_parte(token, chat, &parte)?;
    }
    Ok(())
}

/// Envia UMA mensagem (uma parte já dentro do limite), reenviando em falha transitória.
///
/// A tentativa de rede em si vive em [`tentar_enviar_parte`]; a POLÍTICA de reenvio (quando e
/// quanto esperar) é decidida por [`espera_reenvio`], função pura. A execução (enviar, dormir,
/// logar) é injetada em [`enviar_com_politica`] para ser testável sem rede/relógio/disco.
fn enviar_parte(token: &str, chat: i64, texto: &str) -> Result<(), FalhaProvedor> {
    enviar_com_politica(
        || tentar_enviar_parte(token, chat, texto),
        std::thread::sleep,
        |falha, tentativa, espera| {
            registrar(&format!(
                "[telegram] envio falhou ({falha}); reenviando em {}ms (tentativa {tentativa}/{MAX_RETENTATIVAS_ENVIO})",
                espera.as_millis()
            ));
        },
    )
}

/// UMA tentativa de `sendMessage` (sem reenvio). Devolve falha tipada em qualquer não-2xx.
fn tentar_enviar_parte(token: &str, chat: i64, texto: &str) -> Result<(), FalhaProvedor> {
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

/// Executor do reenvio: tenta `enviar`; em falha, consulta [`espera_reenvio`] e, se mandar
/// retentar, avisa (`ao_retentar`) e `dorme` antes de tentar de novo. Genérico sobre as três
/// ações (enviar/dormir/observar) → testável com mocks, sem tocar rede, relógio nem log.
fn enviar_com_politica(
    mut enviar: impl FnMut() -> Result<(), FalhaProvedor>,
    mut dormir: impl FnMut(Duration),
    mut ao_retentar: impl FnMut(&FalhaProvedor, u32, Duration),
) -> Result<(), FalhaProvedor> {
    let mut tentativa_atual = 0u32;
    loop {
        match enviar() {
            Ok(()) => return Ok(()),
            Err(falha) => match espera_reenvio(&falha, tentativa_atual) {
                Some(espera) => {
                    ao_retentar(&falha, tentativa_atual + 1, espera);
                    dormir(espera);
                    tentativa_atual += 1;
                }
                // Falha não-transitória (ex.: 400 de mensagem malformada) ou orçamento
                // esgotado: desiste devolvendo o erro (sem engolir em silêncio).
                None => return Err(falha),
            },
        }
    }
}

/// Decide a espera antes de RE-enviar uma parte que falhou, ou `None` para desistir.
///
/// Função **pura**. Mais CONSERVADORA que [`FalhaProvedor::vale_retentar`] de propósito: só
/// retenta quando o servidor REJEITOU explicitamente (via [`falha_de_envio_vale_retentar`]) —
/// aí temos certeza de que a mensagem NÃO foi entregue e reenviar não duplica. Num `429` o
/// Telegram costuma dizer QUANTO esperar (`retry_after`); quando diz, honramos esse tempo
/// (limitado por [`TETO_ESPERA_ENVIO_MS`]) em vez do backoff cego.
fn espera_reenvio(falha: &FalhaProvedor, tentativa_atual: u32) -> Option<Duration> {
    if tentativa_atual >= MAX_RETENTATIVAS_ENVIO {
        return None;
    }
    if !falha_de_envio_vale_retentar(falha) {
        return None;
    }
    if let FalhaProvedor::Http { status: 429, corpo } = falha {
        if let Some(segundos) = retry_after_do_corpo(corpo) {
            let ms = segundos.saturating_mul(1000).min(TETO_ESPERA_ENVIO_MS);
            return Some(Duration::from_millis(ms));
        }
    }
    Some(crate::retentativa::espera_backoff(
        tentativa_atual,
        ESPERA_BASE_ENVIO_MS,
    ))
}

/// Uma falha de ENVIO ao Telegram vale reenviar? Só quando o servidor REJEITOU explicitamente
/// (HTTP 429/5xx/408): nesses casos a mensagem com CERTEZA não foi entregue, então reenviar não
/// duplica. Falha de REDE (`Rede`) é AMBÍGUA — a mensagem pode ter chegado antes de a conexão
/// cair — então NÃO reenviamos, para nunca mandar a mesma resposta duas vezes. (Por isso não
/// reusamos [`FalhaProvedor::vale_retentar`], que trata `Rede` como transitória.)
fn falha_de_envio_vale_retentar(falha: &FalhaProvedor) -> bool {
    matches!(
        falha,
        FalhaProvedor::Http { status, .. } if matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
    )
}

/// Extrai o `retry_after` (em segundos) do corpo de erro do Telegram, quando presente.
/// Num `429` o Telegram responde algo como
/// `{"ok":false,"error_code":429,"parameters":{"retry_after":5}}`. Devolve `None` se o corpo
/// não trouxer o campo (ou não for JSON válido) — aí o chamador cai no backoff exponencial.
fn retry_after_do_corpo(corpo: &str) -> Option<u64> {
    let raiz = json::parsear(corpo).ok()?;
    let segundos = raiz
        .obter("parameters")?
        .obter("retry_after")?
        .como_numero()?;
    if segundos.is_finite() && segundos >= 0.0 {
        Some(segundos as u64)
    } else {
        None
    }
}

/// Divide um texto em partes de no máximo `limite` caracteres, preferindo quebrar em uma
/// quebra de linha dentro da janela (resposta mais legível). Se não houver `\n` útil, corta
/// no limite. Texto vazio vira uma única parte vazia (deixa o Telegram recusar com clareza).
pub fn dividir_resposta(texto: &str, limite: usize) -> Vec<String> {
    let total = texto.chars().count();
    if total <= limite {
        return vec![texto.to_string()];
    }

    let mut partes = Vec::new();
    let chars: Vec<char> = texto.chars().collect();
    let mut inicio = 0;
    while inicio < chars.len() {
        let fim_max = (inicio + limite).min(chars.len());
        // Tenta quebrar na última '\n' dentro da janela (sem cortar no meio de uma linha).
        let corte = if fim_max < chars.len() {
            (inicio..fim_max)
                .rev()
                .find(|&i| chars[i] == '\n')
                .map(|i| i + 1) // inclui a quebra na parte atual
                .unwrap_or(fim_max)
        } else {
            fim_max
        };
        let parte: String = chars[inicio..corte].iter().collect();
        partes.push(parte);
        inicio = corte;
    }
    partes
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

    // Memória curta: carrega os últimos turnos deste chat (se ligada na config). Desligada
    // (padrão), volta vazia e o roteamento é idêntico ao de antes. Falha de leitura degrada
    // para "sem memória" DENTRO do módulo (nunca derruba a resposta).
    let historico = if config_roteador.historico.habilitado {
        crate::historico::carregar(&config_roteador.historico, mensagem.chat)
    } else {
        Vec::new()
    };

    // Roteia pela cadeia de fallback. O Ollama local na cauda garante que quase sempre
    // há resposta; só se TUDO falhar mandamos um aviso curto (sem deixar o usuário no vácuo).
    let contexto = Contexto {
        sistema: Some(bot.sistema.clone().unwrap_or_else(sistema_padrao)),
        historico,
    };
    let resposta = match rotear(&mensagem.texto, &contexto, config_roteador) {
        Ok(roteada) => {
            registrar(&format!(
                "[{}] respondido pelo provedor '{}'",
                bot.nome, roteada.provedor
            ));
            // Só uma resposta REAL entra na memória (a de cortesia do ramo de erro NÃO entra —
            // memorizar "não consegui pensar" poluiria o contexto das próximas mensagens).
            if config_roteador.historico.habilitado {
                if let Err(erro) = crate::historico::registrar_troca(
                    &config_roteador.historico,
                    mensagem.chat,
                    &contexto.historico,
                    &mensagem.texto,
                    &roteada.texto,
                ) {
                    // Best-effort: a resposta já vai ser enviada; perder a memória de um turno
                    // é degradação aceitável. Loga (nunca engole em silêncio), mas não falha.
                    registrar(&format!("[{}] não gravei o histórico: {erro}", bot.nome));
                }
            }
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

    #[test]
    fn resposta_curta_fica_em_uma_parte_so() {
        assert_eq!(dividir_resposta("oi", 4000), vec!["oi"]);
        // Exatamente no limite ainda é uma parte.
        let no_limite = "a".repeat(10);
        assert_eq!(dividir_resposta(&no_limite, 10), vec![no_limite]);
    }

    #[test]
    fn resposta_longa_quebra_em_partes_dentro_do_limite() {
        let texto = "a".repeat(25);
        let partes = dividir_resposta(&texto, 10);
        assert_eq!(partes.len(), 3); // 10 + 10 + 5
        assert!(partes.iter().all(|p| p.chars().count() <= 10));
        assert_eq!(partes.concat(), texto); // nada se perde
    }

    #[test]
    fn quebra_prefere_a_quebra_de_linha() {
        // Duas linhas; com limite 8, deve cortar logo após o '\n' (na posição 6), não no meio.
        let texto = "linha\nmais conteudo aqui";
        let partes = dividir_resposta(texto, 8);
        assert_eq!(partes[0], "linha\n");
        assert_eq!(partes.concat(), texto);
    }

    #[test]
    fn quebra_respeita_fronteira_utf8() {
        // Caracteres multibyte não podem ser cortados no meio (contamos por char, não byte).
        let texto = "áéíóú".repeat(4); // 20 chars
        let partes = dividir_resposta(&texto, 7);
        assert!(partes.iter().all(|p| p.chars().count() <= 7));
        assert_eq!(partes.concat(), texto);
    }

    // -- Reenvio ao Telegram em falha transitória --

    fn http(status: u16, corpo: &str) -> FalhaProvedor {
        FalhaProvedor::Http {
            status,
            corpo: corpo.to_string(),
        }
    }

    #[test]
    fn retry_after_extrai_segundos_do_corpo_do_telegram() {
        let corpo = r#"{"ok":false,"error_code":429,"parameters":{"retry_after":7}}"#;
        assert_eq!(retry_after_do_corpo(corpo), Some(7));
    }

    #[test]
    fn retry_after_ausente_ou_invalido_vira_none() {
        assert_eq!(retry_after_do_corpo(r#"{"ok":false}"#), None); // sem parameters
        assert_eq!(retry_after_do_corpo("não é json"), None); // corpo não-JSON
        assert_eq!(
            retry_after_do_corpo(r#"{"parameters":{"outro":1}}"#),
            None // parameters sem retry_after
        );
    }

    #[test]
    fn envio_so_retenta_falha_rejeitada_pelo_servidor() {
        // 429/5xx/408: o servidor rejeitou -> não foi entregue -> vale reenviar.
        assert!(falha_de_envio_vale_retentar(&http(429, "")));
        assert!(falha_de_envio_vale_retentar(&http(503, "")));
        assert!(falha_de_envio_vale_retentar(&http(408, "")));
        // 400/404: problema DA mensagem -> reenviar não conserta -> não retenta.
        assert!(!falha_de_envio_vale_retentar(&http(400, "")));
        assert!(!falha_de_envio_vale_retentar(&http(404, "")));
        // Rede é AMBÍGUA (pode ter chegado) -> não reenvia, pra não duplicar a resposta.
        assert!(!falha_de_envio_vale_retentar(&FalhaProvedor::Rede(
            "caiu".into()
        )));
    }

    #[test]
    fn espera_reenvio_honra_retry_after_com_teto() {
        // 429 com retry_after pequeno: usa exatamente o tempo pedido.
        let f = http(429, r#"{"parameters":{"retry_after":3}}"#);
        assert_eq!(espera_reenvio(&f, 0), Some(Duration::from_millis(3000)));
        // 429 com retry_after absurdo: satura no teto (não trava a thread por minutos).
        let f = http(429, r#"{"parameters":{"retry_after":99999}}"#);
        assert_eq!(
            espera_reenvio(&f, 0),
            Some(Duration::from_millis(TETO_ESPERA_ENVIO_MS))
        );
    }

    #[test]
    fn espera_reenvio_usa_backoff_quando_sem_retry_after() {
        // 500 (sem retry_after): backoff exponencial base 500 -> 500, 1000...
        assert_eq!(
            espera_reenvio(&http(500, ""), 0),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            espera_reenvio(&http(500, ""), 1),
            Some(Duration::from_millis(1000))
        );
    }

    #[test]
    fn espera_reenvio_desiste_por_orcamento_ou_falha_definitiva() {
        // Orçamento esgotado (já reenviou MAX vezes): desiste.
        assert_eq!(espera_reenvio(&http(500, ""), MAX_RETENTATIVAS_ENVIO), None);
        // Falha definitiva (400): desiste na hora, mesmo com orçamento.
        assert_eq!(espera_reenvio(&http(400, ""), 0), None);
    }

    /// Executor com mocks: conta tentativas de envio e de "sono", sem tocar rede/relógio/log.
    fn rodar_politica(
        respostas: Vec<Result<(), FalhaProvedor>>,
    ) -> (Result<(), FalhaProvedor>, usize, usize) {
        let mut fila = respostas.into_iter();
        let mut enviados = 0usize;
        let mut dormidas = 0usize;
        let resultado = enviar_com_politica(
            || {
                enviados += 1;
                fila.next().unwrap_or(Ok(()))
            },
            |_espera| dormidas += 1,
            |_falha, _tentativa, _espera| {}, // observador silencioso no teste
        );
        (resultado, enviados, dormidas)
    }

    #[test]
    fn politica_entrega_de_primeira_nao_reenvia() {
        let (r, enviados, dormidas) = rodar_politica(vec![Ok(())]);
        assert!(r.is_ok());
        assert_eq!(enviados, 1); // uma tentativa só
        assert_eq!(dormidas, 0); // nenhum reenvio
    }

    #[test]
    fn politica_reenvia_e_entrega_apos_blip() {
        // Falha 503 na 1ª, entrega na 2ª: uma retentativa, uma "dormida".
        let (r, enviados, dormidas) = rodar_politica(vec![Err(http(503, "")), Ok(())]);
        assert!(r.is_ok());
        assert_eq!(enviados, 2);
        assert_eq!(dormidas, 1);
    }

    #[test]
    fn politica_desiste_apos_orcamento_em_falha_persistente() {
        // 503 sempre: tenta a original + MAX reenvios, depois devolve o erro (sem engolir).
        let sempre_503 = vec![http(503, ""), http(503, ""), http(503, ""), http(503, "")]
            .into_iter()
            .map(Err)
            .collect();
        let (r, enviados, dormidas) = rodar_politica(sempre_503);
        assert!(r.is_err());
        assert_eq!(enviados as u32, MAX_RETENTATIVAS_ENVIO + 1);
        assert_eq!(dormidas as u32, MAX_RETENTATIVAS_ENVIO);
    }

    #[test]
    fn politica_nao_reenvia_falha_definitiva() {
        // 400 (mensagem malformada): desiste na 1ª, sem reenvio nem sono.
        let (r, enviados, dormidas) = rodar_politica(vec![Err(http(400, ""))]);
        assert!(r.is_err());
        assert_eq!(enviados, 1);
        assert_eq!(dormidas, 0);
    }
}
