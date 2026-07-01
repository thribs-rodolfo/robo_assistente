//! Teste de integração AO VIVO do LOOP de memória curta, contra o Ollama local.
//!
//! Prova o ciclo completo que a ponte executa: carregar histórico do disco → rotear com ele
//! como contexto → gravar a nova troca. Na 2ª rodada, o histórico gravado na 1ª tem de estar
//! lá e ser passado ao provedor.
//!
//! Marcado `#[ignore]` porque depende do Ollama (127.0.0.1:11434) e é lento. Rode sob demanda:
//!
//!   cargo test --test integracao_historico -- --ignored --nocapture
//!
//! NÃO dispara o Claude (a ordem tem só o Ollama), respeitando a regra de nunca acionar o
//! refresh do token "só pra testar".

use std::time::Duration;

use roteador::config::{Config, ConfigHistorico, ConfigProvedor};
use roteador::{historico, rotear, Contexto};

#[test]
#[ignore = "depende do Ollama local vivo"]
fn loop_de_memoria_curta_persiste_e_reusa_o_historico() {
    // Diretório de histórico temporário e isolado (não toca o de produção).
    let dir = std::env::temp_dir()
        .join(format!("roteador-integracao-hist-{}", std::process::id()))
        .to_string_lossy()
        .to_string();
    let _ = std::fs::remove_dir_all(&dir);

    let cfg_hist = ConfigHistorico {
        habilitado: true,
        diretorio: dir.clone(),
        max_turnos: 6,
        max_chars_por_turno: 2000,
    };

    let config = Config {
        ordem_fallback: vec!["ollama_local".to_string()],
        provedores: vec![ConfigProvedor {
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
        }],
        disjuntor: Default::default(),
        historico: cfg_hist.clone(),
        telemetria_log: std::env::temp_dir()
            .join("roteador-integracao-hist.log")
            .to_string_lossy()
            .to_string(),
    };

    let chat: i64 = 555_001;

    // --- Rodada 1: conversa nova (sem histórico) ---
    let hist1 = historico::carregar(&cfg_hist, chat);
    assert!(hist1.is_empty(), "chat novo começa sem memória");
    let contexto1 = Contexto {
        sistema: Some("Você é um assistente conciso.".into()),
        historico: hist1,
    };
    let r1 = rotear(
        "Meu nome é Rodolfo. Só confirme com 'ok'.",
        &contexto1,
        &config,
    )
    .expect("rodada 1 devia rotear pelo Ollama");
    assert_eq!(r1.provedor, "ollama_local");
    historico::registrar_troca(
        &cfg_hist,
        chat,
        &contexto1.historico,
        "Meu nome é Rodolfo. Só confirme com 'ok'.",
        &r1.texto,
    )
    .expect("gravar a troca 1");

    // --- Rodada 2: a memória da rodada 1 tem de voltar do disco ---
    let hist2 = historico::carregar(&cfg_hist, chat);
    assert_eq!(
        hist2.len(),
        2,
        "a troca 1 (usuário+assistente) foi persistida"
    );
    assert!(
        hist2[0].texto.contains("Rodolfo"),
        "o turno do usuário guardado deve trazer o nome; veio: {:?}",
        hist2[0].texto
    );
    let contexto2 = Contexto {
        sistema: Some("Você é um assistente conciso.".into()),
        historico: hist2,
    };
    let r2 = rotear("Qual é o meu nome?", &contexto2, &config)
        .expect("rodada 2 devia rotear pelo Ollama");
    assert_eq!(r2.provedor, "ollama_local");
    // NÃO exigimos que o modelo pequeno acerte o nome (qwen2.5:1.5b é fraco); o que este teste
    // garante é o LOOP: o histórico foi persistido e RECARREGADO para virar contexto da 2ª
    // mensagem. A qualidade da resposta é responsabilidade do modelo, não do roteador.
    assert!(
        !r2.texto.trim().is_empty(),
        "a rodada 2 devia responder algo"
    );

    historico::registrar_troca(
        &cfg_hist,
        chat,
        &contexto2.historico,
        "Qual é o meu nome?",
        &r2.texto,
    )
    .expect("gravar a troca 2");

    // Depois de duas trocas, há 4 turnos guardados (memória curta acumulando).
    let hist3 = historico::carregar(&cfg_hist, chat);
    assert_eq!(hist3.len(), 4, "duas trocas = quatro turnos");

    let _ = std::fs::remove_dir_all(&dir);
}
