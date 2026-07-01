//! Teste de integração AO VIVO contra o Ollama local.
//!
//! Marcado `#[ignore]` porque depende de um serviço externo (Ollama em
//! 127.0.0.1:11434) e é lento. Rode sob demanda:
//!
//!   cargo test --test integracao_ollama -- --ignored --nocapture
//!
//! Importante: este teste NÃO dispara o Claude (a ordem tem só o Ollama), respeitando
//! a regra de nunca acionar o refresh do token "só pra testar".

use std::time::Duration;

use roteador::config::{Config, ConfigProvedor};
use roteador::{rotear, Contexto};

#[test]
#[ignore = "depende do Ollama local vivo"]
fn ollama_local_responde_de_verdade() {
    // Config mínima: só o Ollama, para isolar o caminho feliz sem outros provedores.
    let config = Config {
        ordem_fallback: vec!["ollama_local".to_string()],
        provedores: vec![ConfigProvedor {
            nome: "ollama_local".into(),
            tipo: "ollama".into(),
            url_base: Some("http://127.0.0.1:11434".into()),
            modelo: Some("qwen2.5:1.5b".into()),
            comando: None,
            chave: None,
            timeout: Duration::from_secs(120),
            habilitado: true,
        }],
        disjuntor: Default::default(),
    };

    let resposta = rotear(
        "Responda em uma palavra: qual a capital da França?",
        &Contexto::vazio(),
        &config,
    )
    .expect("o Ollama local deveria responder");

    println!("[provedor: {}] {}", resposta.provedor, resposta.texto);
    assert_eq!(resposta.provedor, "ollama_local");
    assert!(!resposta.texto.is_empty(), "resposta não pode ser vazia");
}
