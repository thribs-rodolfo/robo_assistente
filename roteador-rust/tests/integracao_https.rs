//! Teste de integração AO VIVO do transporte HTTPS (via curl).
//!
//! `#[ignore]`: depende de rede. Rode sob demanda:
//!   cargo test --test integracao_https -- --ignored --nocapture
//!
//! Prova que o cliente HTTPS fala TLS de verdade e parseia o status, SEM precisar de chave
//! válida e SEM tocar no Claude: batemos no endpoint real do Gemini com uma chave inválida,
//! e esperamos um erro HTTP estruturado (status 4xx), não um pânico nem sucesso falso.

use std::time::Duration;

use roteador::erro::FalhaProvedor;
use roteador::https;

#[test]
#[ignore = "depende de rede"]
fn https_devolve_status_estruturado_com_chave_invalida() {
    let url = "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:generateContent?key=CHAVE_INVALIDA_DE_TESTE";
    let corpo = r#"{"contents":[{"parts":[{"text":"oi"}]}]}"#;

    let resultado = https::post_json(url, corpo, &[], Duration::from_secs(20));
    println!("resultado: {resultado:?}");

    match resultado {
        // O esperado: o transporte funcionou e o Google recusou a chave (tipicamente 400).
        Ok(resposta) => {
            assert!(
                (400..500).contains(&resposta.status),
                "esperava 4xx por chave inválida, veio {}",
                resposta.status
            );
        }
        // Também aceitamos um Http tipado (caso o provedor caísse aqui por outro caminho).
        Err(FalhaProvedor::Http { status, .. }) => {
            assert!((400..500).contains(&status), "esperava 4xx, veio {status}");
        }
        outro => panic!("esperava resposta HTTP estruturada, veio {outro:?}"),
    }
}
