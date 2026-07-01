//! Testes de integração do binário `verificar-config`.
//!
//! Sobem o binário REAL sobre arquivos de config temporários e conferem o CONTRATO de linha
//! de comando: código de saída (0 sem erro / 1 com erro ou falha ao ler) e o texto impresso.
//! Fixam o que provei à mão contra a config de produção.
//!
//! Diferente dos outros testes de integração (ponte/ollama/disjuntor), estes **não abrem
//! socket nem tocam rede** — o `verificar-config` só lê um arquivo e roda funções puras. Por
//! isso são herméticos e rodam por padrão (sem `#[ignore]`): nada de Ollama, nada de Telegram,
//! e o Claude JAMAIS é tocado (ver licao-refresh-token-rotativo).

use std::process::Command;

/// Escreve um arquivo temporário com nome único e devolve o caminho.
fn escrever_temp(nome: &str, conteudo: &str) -> String {
    let caminho = std::env::temp_dir().join(nome);
    std::fs::write(&caminho, conteudo).expect("escrever config de teste");
    caminho.to_string_lossy().into_owned()
}

/// Roda o binário `verificar-config` com um caminho e devolve `(codigo_saida, stdout)`.
/// `codigo_saida` é `None` se o processo foi morto por sinal (não deve acontecer aqui).
fn rodar(caminho: &str) -> (Option<i32>, String) {
    let binario = env!("CARGO_BIN_EXE_verificar-config");
    let saida = Command::new(binario)
        .arg(caminho)
        .output()
        .expect("rodar o binário verificar-config");
    let stdout = String::from_utf8_lossy(&saida.stdout).into_owned();
    (saida.status.code(), stdout)
}

/// Config saudável (piso Ollama local habilitado): sai 0 e diz que está tudo certo.
#[test]
fn config_saudavel_sai_zero() {
    let caminho = escrever_temp(
        "verificar-config-saudavel.json",
        r#"{"ordem_fallback":["claude","ollama_local"],
            "provedores":{
                "claude":{"tipo":"claude_cli","comando":"claude"},
                "ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b"}
            }}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(
        codigo,
        Some(0),
        "config saudável devia sair 0. stdout:\n{stdout}"
    );
    assert!(stdout.contains("Tudo certo"), "stdout:\n{stdout}");
}

/// Espelha a config de PRODUÇÃO (groq/gemini declarados mas fora da ordem): funciona (sai 0),
/// mas emite avisos de "declarado mas não está na ordem_fallback".
#[test]
fn provedor_fora_da_ordem_sai_zero_com_aviso() {
    let caminho = escrever_temp(
        "verificar-config-producao-like.json",
        r#"{"ordem_fallback":["claude","ollama_local"],
            "provedores":{
                "claude":{"tipo":"claude_cli"},
                "groq":{"tipo":"openai_compat","url_base":"https://api.groq.com/openai/v1","modelo":"m"},
                "gemini":{"tipo":"gemini_rest","modelo":"m"},
                "ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b"}
            }}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(
        codigo,
        Some(0),
        "só avisos devem manter exit 0. stdout:\n{stdout}"
    );
    assert!(stdout.contains("AVISO"), "stdout:\n{stdout}");
    assert!(stdout.contains("nunca será usado"), "stdout:\n{stdout}");
}

/// Piso perigoso (Groq, que exige chave, como último da ordem): é ERRO → sai 1.
#[test]
fn piso_inseguro_sai_um() {
    let caminho = escrever_temp(
        "verificar-config-piso-inseguro.json",
        r#"{"ordem_fallback":["claude","groq"],
            "provedores":{
                "claude":{"tipo":"claude_cli"},
                "groq":{"tipo":"openai_compat","url_base":"https://api.groq.com/openai/v1","modelo":"m","habilitado":true}
            }}"#,
    );
    let (codigo, stdout) = rodar(&caminho);
    assert_eq!(
        codigo,
        Some(1),
        "piso inseguro devia sair 1. stdout:\n{stdout}"
    );
    assert!(stdout.contains("ERRO"), "stdout:\n{stdout}");
    assert!(stdout.contains("EXIGE chave"), "stdout:\n{stdout}");
}

/// Arquivo inexistente: falha de leitura também é falha de config → sai 1 (sem panic).
#[test]
fn arquivo_inexistente_sai_um() {
    let caminho = std::env::temp_dir()
        .join("verificar-config-nao-existe-xyz.json")
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&caminho); // garante que não existe
    let (codigo, _stdout) = rodar(&caminho);
    assert_eq!(codigo, Some(1), "arquivo inexistente devia sair 1");
}
