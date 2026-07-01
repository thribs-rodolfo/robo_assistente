//! Teste de integração AO VIVO do disjuntor sobre uma cadeia REAL.
//!
//! Marcado `#[ignore]` porque depende do Ollama local (127.0.0.1:11434). Rode sob demanda:
//!
//!   cargo test --test integracao_disjuntor -- --ignored --nocapture
//!
//! Prova, ponta a ponta, que o disjuntor ABRE o circuito de um provedor que falha em série
//! numa cadeia de verdade — sem NUNCA tocar no Claude (a cadeia é [porta-morta, Ollama]).
//! O topo aponta para uma porta TCP morta (falha rápido); o piso é o Ollama real, que
//! responde. Depois de `limiar_falhas` roteamentos, o topo deve ficar aberto no estado.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use roteador::config::{Config, ConfigDisjuntor, ConfigProvedor};
use roteador::disjuntor::EstadoDisjuntor;
use roteador::{rotear, Contexto};

fn epoch_agora() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[test]
#[ignore = "depende do Ollama local vivo"]
fn disjuntor_abre_o_topo_falho_em_cadeia_real() {
    let caminho_estado = std::env::temp_dir().join("roteador-disjuntor-integracao.estado");
    let caminho_estado = caminho_estado.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&caminho_estado);

    // Cadeia real: topo morto (porta 1) -> Ollama local (piso, responde de verdade).
    // Disjuntor ligado, abre com 2 falhas seguidas, cooldown longo para a asserção.
    let config = Config {
        ordem_fallback: vec!["topo_morto".to_string(), "ollama_local".to_string()],
        provedores: vec![
            ConfigProvedor {
                nome: "topo_morto".into(),
                tipo: "ollama".into(),
                url_base: Some("http://127.0.0.1:1".into()),
                modelo: Some("m".into()),
                comando: None,
                chave: None,
                timeout: Duration::from_secs(2),
                habilitado: true,
            },
            ConfigProvedor {
                nome: "ollama_local".into(),
                tipo: "ollama".into(),
                url_base: Some("http://127.0.0.1:11434".into()),
                modelo: Some("qwen2.5:1.5b".into()),
                comando: None,
                chave: None,
                timeout: Duration::from_secs(120),
                habilitado: true,
            },
        ],
        disjuntor: ConfigDisjuntor {
            habilitado: true,
            limiar_falhas: 2,
            cooldown_segundos: 3_600,
            cooldown_maximo_segundos: 86_400,
            caminho_estado: caminho_estado.clone(),
        },
        // Telemetria para arquivo temporário: não suja o log de produção (fonte das métricas).
        telemetria_log: std::env::temp_dir()
            .join("roteador-integracao-disjuntor.log")
            .to_string_lossy()
            .to_string(),
    };

    // Duas rodadas: o topo morto falha nas duas; o Ollama (piso) responde nas duas.
    for rodada in 1..=2 {
        let resposta = rotear(
            "Responda em uma palavra: qual a capital da França?",
            &Contexto::vazio(),
            &config,
        )
        .expect("o Ollama piso deveria responder");
        println!("[rodada {rodada}] respondeu: {}", resposta.provedor);
        assert_eq!(resposta.provedor, "ollama_local");
    }

    // Depois de 2 falhas seguidas, o circuito do topo tem que estar ABERTO no estado gravado.
    let estado = EstadoDisjuntor::carregar(&caminho_estado);
    assert!(
        estado.esta_aberto("topo_morto", epoch_agora()),
        "topo_morto deveria estar com o circuito aberto após 2 falhas"
    );
    // E o piso, que respondeu, não pode estar aberto (sucesso fecha/limpa).
    assert!(!estado.esta_aberto("ollama_local", epoch_agora()));

    // Terceira rodada: com o topo aberto, o roteador o PULA e vai direto ao Ollama.
    let resposta = rotear("Diga: ok.", &Contexto::vazio(), &config)
        .expect("piso responde mesmo com o topo pulado");
    assert_eq!(resposta.provedor, "ollama_local");
    println!(
        "[rodada 3] topo pulado pelo disjuntor; respondeu: {}",
        resposta.provedor
    );

    let _ = std::fs::remove_file(&caminho_estado);
}
