//! Teste de integração AO VIVO do MODO SOMBRA do disjuntor contra o Ollama local.
//!
//! Marcado `#[ignore]` porque depende de um serviço externo (Ollama em 127.0.0.1:11434)
//! e é lento. Rode sob demanda:
//!
//!   cargo test --test integracao_sombra -- --ignored --nocapture
//!
//! O que ele prova (o valor do modo sombra):
//! - A cadeia é [morto, ollama_local], com o 'morto' PRÉ-MARCADO como circuito ABERTO no
//!   arquivo de estado do disjuntor, e `disjuntor.sombra = true`.
//! - No modo ATIVO, o 'morto' seria PULADO. No modo SOMBRA, ele é TENTADO mesmo assim
//!   (roteamento idêntico ao de hoje) — falha (porta morta) — e o Ollama responde por baixo.
//! - A telemetria ganha uma linha `[disjuntor-sombra] pularia 'morto' ... teria economizado`:
//!   é o disjuntor mostrando o que FARIA, com risco zero, contra tráfego real.
//!
//! Importante: a ordem NÃO tem Claude → este teste NUNCA dispara o refresh do token
//! (licao-refresh-token-rotativo). Só o Ollama local (custo zero) é acionado de verdade.

use std::time::Duration;

use roteador::config::{Config, ConfigDisjuntor, ConfigProvedor};
use roteador::disjuntor::EstadoDisjuntor;
use roteador::{rotear, Contexto};

#[test]
#[ignore = "depende do Ollama local vivo"]
fn modo_sombra_ao_vivo_nao_pula_e_registra_a_economia() {
    // Arquivo de estado temporário com o 'morto' já ABERTO (falhando em série). Ancoramos as
    // falhas no AGORA real porque o `rotear` usa o relógio real ao checar `esta_aberto`.
    let caminho_estado = std::env::temp_dir()
        .join("roteador-integracao-sombra.estado")
        .to_string_lossy()
        .to_string();
    let _ = std::fs::remove_file(&caminho_estado);

    let cfg_dj = ConfigDisjuntor {
        habilitado: true,
        sombra: true,
        limiar_falhas: 3,
        cooldown_segundos: 100_000,
        cooldown_maximo_segundos: 1_000_000,
        caminho_estado: caminho_estado.clone(),
    };
    let agora_real = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut estado = EstadoDisjuntor::vazio();
    for _ in 0..3 {
        estado.apos_falha("morto", agora_real, &cfg_dj);
    }
    estado.salvar(&caminho_estado);

    // Log de telemetria próprio (temporário): não suja o log de produção nem as métricas.
    let telemetria_log = std::env::temp_dir()
        .join("roteador-integracao-sombra.log")
        .to_string_lossy()
        .to_string();
    let _ = std::fs::remove_file(&telemetria_log);

    // Cadeia [morto (porta morta), ollama_local (real)]. O 'morto' está com o circuito aberto,
    // mas em modo sombra ele NÃO é pulado — é tentado, falha, e o Ollama responde por baixo.
    let config = Config {
        ordem_fallback: vec!["morto".to_string(), "ollama_local".to_string()],
        provedores: vec![
            ConfigProvedor {
                nome: "morto".into(),
                tipo: "ollama".into(),
                url_base: Some("http://127.0.0.1:1".into()),
                modelo: Some("m".into()),
                comando: None,
                chave: None,
                mensagem_fixa: None,
                timeout: Duration::from_secs(2),
                habilitado: true,
                retentativas: 0,
                retentativa_espera_ms: 250,
            },
            ConfigProvedor {
                nome: "ollama_local".into(),
                tipo: "ollama".into(),
                url_base: Some("http://127.0.0.1:11434".into()),
                modelo: Some("qwen2.5:1.5b".into()),
                comando: None,
                chave: None,
                mensagem_fixa: None,
                timeout: Duration::from_secs(120),
                habilitado: true,
                retentativas: 0,
                retentativa_espera_ms: 250,
            },
        ],
        disjuntor: cfg_dj,
        telemetria_log: telemetria_log.clone(),
    };

    let resposta = rotear(
        "Responda em uma palavra: qual a capital da França?",
        &Contexto::vazio(),
        &config,
    )
    .expect("o Ollama local deveria responder por baixo do 'morto'");

    println!("[provedor: {}] {}", resposta.provedor, resposta.texto);
    // Quem respondeu foi o Ollama — o 'morto' foi TENTADO (não pulado) e falhou antes.
    assert_eq!(resposta.provedor, "ollama_local");
    assert!(!resposta.texto.is_empty(), "resposta não pode ser vazia");

    // A telemetria deve conter a linha da SOMBRA: o disjuntor mostrando o que faria.
    let conteudo = std::fs::read_to_string(&telemetria_log).unwrap_or_default();
    println!("--- telemetria ---\n{conteudo}");
    assert!(
        conteudo.contains("[disjuntor-sombra] pularia 'morto'"),
        "faltou a linha de sombra; veio:\n{conteudo}"
    );
    // E NÃO pode ter a linha do modo ATIVO (pulo real): a sombra não pula.
    assert!(
        !conteudo.contains("[disjuntor] morto: disjuntor aberto"),
        "a sombra não pode emitir a linha de pulo real; veio:\n{conteudo}"
    );

    let _ = std::fs::remove_file(&caminho_estado);
    let _ = std::fs::remove_file(&telemetria_log);
}
