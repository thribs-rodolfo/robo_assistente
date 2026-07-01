//! Testes de integração do binário `diagnostico` (checagem de vivacidade do piso).
//!
//! Sobem o binário REAL sobre configs temporárias e fixam o CONTRATO de linha de comando:
//! código de saída (0 piso vivo / 1 comprometido / 2 não-verificado) e o texto impresso.
//! Capturam o que provei à mão contra a config de produção.
//!
//! Herméticos e rodam por PADRÃO (sem `#[ignore]`): os casos aqui ou não tocam a rede
//! (piso desabilitado, piso não-Ollama, arquivo inexistente) ou usam uma porta MORTA
//! (`127.0.0.1:1` → "connection refused" imediato, sem depender de serviço externo). O
//! caminho SAUDÁVEL exige o Ollama vivo, então mora no teste `#[ignore]` no fim.
//! O Claude JAMAIS é tocado (ver licao-refresh-token-rotativo): o único tipo que este
//! binário sonda é `ollama`.

use std::process::Command;

/// Escreve um arquivo temporário e devolve o caminho.
fn escrever_temp(nome: &str, conteudo: &str) -> String {
    let caminho = std::env::temp_dir().join(nome);
    std::fs::write(&caminho, conteudo).expect("escrever config de teste");
    caminho.to_string_lossy().into_owned()
}

/// Roda o binário `diagnostico` com um caminho e devolve `(codigo_saida, stdout)`.
fn rodar(caminho: &str) -> (Option<i32>, String) {
    let binario = env!("CARGO_BIN_EXE_diagnostico");
    let saida = Command::new(binario)
        .arg(caminho)
        .output()
        .expect("rodar o binário diagnostico");
    let stdout = String::from_utf8_lossy(&saida.stdout).into_owned();
    (saida.status.code(), stdout)
}

/// Piso Ollama com `habilitado:false`: comprometido → sai 1 (sem abrir socket).
#[test]
fn piso_desabilitado_sai_um() {
    let caminho = escrever_temp(
        "diagnostico-desabilitado.json",
        r#"{"ordem_fallback":["piso"],
            "provedores":{"piso":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"m","habilitado":false}}}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(
        codigo,
        Some(1),
        "piso desabilitado devia sair 1. stdout:\n{stdout}"
    );
    assert!(stdout.contains("DESABILITADO"), "stdout:\n{stdout}");
}

/// Piso Ollama apontando para uma porta MORTA: fora do ar → sai 1 (hermético, sem Ollama).
#[test]
fn piso_fora_do_ar_sai_um() {
    let caminho = escrever_temp(
        "diagnostico-morto.json",
        r#"{"ordem_fallback":["piso"],
            "provedores":{"piso":{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":2}}}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(
        codigo,
        Some(1),
        "porta morta devia sair 1. stdout:\n{stdout}"
    );
    assert!(stdout.contains("fora do ar"), "stdout:\n{stdout}");
}

/// Piso NÃO-Ollama (Claude): não sondamos → sai 2 (Claude jamais disparado).
#[test]
fn piso_nao_ollama_sai_dois() {
    let caminho = escrever_temp(
        "diagnostico-claude.json",
        r#"{"ordem_fallback":["claude"],"provedores":{"claude":{"tipo":"claude_cli"}}}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(
        codigo,
        Some(2),
        "piso não-Ollama devia sair 2. stdout:\n{stdout}"
    );
    assert!(stdout.contains("não sondei"), "stdout:\n{stdout}");
}

/// Arquivo inexistente: falha de leitura da config → sai 1 (sem panic).
#[test]
fn arquivo_inexistente_sai_um() {
    let caminho = std::env::temp_dir()
        .join("diagnostico-nao-existe-xyz.json")
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&caminho);
    let (codigo, _stdout) = rodar(&caminho);
    assert_eq!(codigo, Some(1), "arquivo inexistente devia sair 1");
}

/// Caminho SAUDÁVEL: exige o Ollama local vivo com o modelo instalado → sai 0.
/// `#[ignore]` porque depende de serviço externo (rode com `cargo test -- --ignored`).
#[test]
#[ignore = "depende do Ollama local vivo em 127.0.0.1:11434 com qwen2.5:1.5b"]
fn piso_vivo_sai_zero() {
    let caminho = escrever_temp(
        "diagnostico-vivo.json",
        r#"{"ordem_fallback":["ollama_local"],
            "provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b","timeout_segundos":10}}}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(codigo, Some(0), "piso vivo devia sair 0. stdout:\n{stdout}");
    assert!(stdout.contains("vivo"), "stdout:\n{stdout}");
}
