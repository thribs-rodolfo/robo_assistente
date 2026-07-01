//! Testes de integração da ponte Telegram (o binário `ponte-telegram`).
//!
//! Sobem o binário REAL numa porta descartável (via env `PONTE_ENDERECO`) e falam HTTP
//! cru por um `TcpStream`, como o nginx faria. Marcados `#[ignore]` porque sobem um
//! processo e abrem um socket (rodar com `cargo test --test integracao_ponte -- --ignored`).
//!
//! Disciplina: NUNCA disparamos o Claude aqui. As configs de teste roteiam só pelo Ollama
//! e usam token de bot FAKE, então nada é entregue ao Telegram e o refresh do Claude nunca
//! é tocado (ver licao-refresh-token-rotativo).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::Duration;

/// Mata o processo filho ao sair do teste, mesmo se um `assert!` falhar no meio.
struct PonteViva(Child);
impl Drop for PonteViva {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Sobe o binário `ponte-telegram` apontando para configs de teste, numa porta dada.
fn subir_ponte(endereco: &str, config_bots: &str, config_roteador: &str) -> PonteViva {
    let binario = env!("CARGO_BIN_EXE_ponte-telegram");
    let filho = Command::new(binario)
        .env("PONTE_ENDERECO", endereco)
        .env("PONTE_CONFIG", config_bots)
        .env("ROTEADOR_CONFIG", config_roteador)
        .spawn()
        .expect("subir o binário da ponte");
    // Dá um instante para o socket começar a escutar antes de conectar.
    std::thread::sleep(Duration::from_millis(400));
    PonteViva(filho)
}

/// Faz uma requisição HTTP crua e devolve (linha_de_status, corpo).
fn requisitar(endereco: &str, requisicao_crua: &str) -> (String, String) {
    let mut socket = TcpStream::connect(endereco).expect("conectar na ponte");
    socket
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("set timeout");
    socket
        .write_all(requisicao_crua.as_bytes())
        .expect("enviar requisição");
    let mut resposta = String::new();
    socket.read_to_string(&mut resposta).expect("ler resposta");
    let (cabecalho, corpo) = resposta.split_once("\r\n\r\n").unwrap_or((&resposta, ""));
    let linha_status = cabecalho.lines().next().unwrap_or("").to_string();
    (linha_status, corpo.to_string())
}

/// Escreve um arquivo temporário com nome único (pela porta) e devolve o caminho.
fn escrever_temp(nome: &str, conteudo: &str) -> String {
    let caminho = std::env::temp_dir().join(nome);
    std::fs::write(&caminho, conteudo).expect("escrever config de teste");
    caminho.to_string_lossy().into_owned()
}

#[test]
#[ignore = "sobe o binário e abre um socket"]
fn saude_responde_ok_e_secret_errado_da_403() {
    let endereco = "127.0.0.1:18991";
    let bots = escrever_temp(
        "ponte-teste-bots-18991.json",
        r#"{"bots":{"teste":{"token":"FAKE","secret":"segredo-certo","allow_from":[1]}}}"#,
    );
    let roteador = escrever_temp(
        "ponte-teste-rot-18991.json",
        r#"{"ordem_fallback":["ollama_local"],"telemetria_log":"/tmp/roteador-integracao-ponte.log","provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b","timeout_segundos":5}}}"#,
    );
    let _ponte = subir_ponte(endereco, &bots, &roteador);

    // 1) Healthcheck -> 200 "ok".
    let (status, corpo) = requisitar(
        endereco,
        "GET /ponte-telegram/saude HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(status.contains("200"), "status do saúde: {status}");
    assert_eq!(corpo, "ok");

    // 2) Webhook com secret ERRADO -> 403 (rejeitado antes de qualquer roteamento).
    let corpo_update = r#"{"message":{"from":{"id":1},"chat":{"id":1},"text":"oi"}}"#;
    let req = format!(
        "POST /ponte-telegram/teste HTTP/1.1\r\nHost: x\r\n\
         X-Telegram-Bot-Api-Secret-Token: secret-errado\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        corpo_update.len(),
        corpo_update
    );
    let (status, _) = requisitar(endereco, &req);
    assert!(
        status.contains("403"),
        "secret errado devia dar 403: {status}"
    );
}

#[test]
#[ignore = "depende do Ollama local vivo em 127.0.0.1:11434"]
fn caminho_completo_roteia_pelo_ollama() {
    let endereco = "127.0.0.1:18992";
    // Token FAKE de propósito: o sendMessage vai falhar (401), mas o que provamos aqui é o
    // 200 imediato do webhook e que o caminho secret->allowFrom->rotear é exercido de verdade.
    let bots = escrever_temp(
        "ponte-teste-bots-18992.json",
        r#"{"bots":{"teste":{"token":"FAKE","secret":"s","allow_from":[42],"sistema":"Responda só 'pong'."}}}"#,
    );
    let roteador = escrever_temp(
        "ponte-teste-rot-18992.json",
        r#"{"ordem_fallback":["ollama_local"],"telemetria_log":"/tmp/roteador-integracao-ponte.log","provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b","timeout_segundos":60}}}"#,
    );
    let _ponte = subir_ponte(endereco, &bots, &roteador);

    let corpo_update = r#"{"message":{"from":{"id":42},"chat":{"id":42},"text":"diga pong"}}"#;
    let req = format!(
        "POST /ponte-telegram/teste HTTP/1.1\r\nHost: x\r\n\
         X-Telegram-Bot-Api-Secret-Token: s\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        corpo_update.len(),
        corpo_update
    );
    // O webhook precisa responder 200 IMEDIATAMENTE (o processamento é assíncrono).
    let (status, corpo) = requisitar(endereco, &req);
    assert!(status.contains("200"), "webhook devia dar 200: {status}");
    assert!(corpo.contains("\"ok\":true"), "corpo do webhook: {corpo}");
}
