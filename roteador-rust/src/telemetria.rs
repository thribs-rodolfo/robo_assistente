//! Telemetria simples: registra qual provedor respondeu e por que a cadeia caiu.
//!
//! Objetivo prático: medir a dependência REAL de cada provedor ao longo do tempo
//! (quantas vezes caímos no Ollama? o Claude está estável?). É só um append a um arquivo
//! de log — sem dependência. Erro de log NÃO derruba o roteador (logar é melhor-esforço),
//! mas também não é silencioso: em falha de escrita, avisamos no stderr.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Caminho do log de telemetria. Mesmo arquivo que o roteador Python usava, para continuidade.
pub const ARQUIVO_LOG: &str = "/var/log/roteador-provedores.log";

/// Anexa uma linha ao log, prefixada com o instante (epoch em segundos).
///
/// Best-effort: se não conseguir abrir/escrever o arquivo (ex.: sem permissão em testes),
/// cai para o stderr em vez de falhar — telemetria nunca deve quebrar o roteamento.
pub fn registrar(mensagem: &str) {
    registrar_em(ARQUIVO_LOG, mensagem);
}

/// Versão testável: registra em um caminho específico.
pub fn registrar_em(caminho: &str, mensagem: &str) {
    let agora = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let linha = format!("{agora} [roteador] {mensagem}\n");

    let resultado = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(caminho)
        .and_then(|mut arquivo| arquivo.write_all(linha.as_bytes()));

    // Sem erro silencioso: se o log falhar, ao menos avisa no stderr (não derruba o fluxo).
    if let Err(erro) = resultado {
        eprintln!("[telemetria] não consegui escrever em {caminho}: {erro}");
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn registra_linha_no_arquivo_temporario() {
        let caminho = std::env::temp_dir().join("roteador-telemetria-teste.log");
        let caminho = caminho.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&caminho);

        registrar_em(&caminho, "[ok] respondido por 'teste'");
        let conteudo = std::fs::read_to_string(&caminho).unwrap();
        assert!(conteudo.contains("respondido por 'teste'"));

        let _ = std::fs::remove_file(&caminho);
    }
}
