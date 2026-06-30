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
    let linha = format!("{} [roteador] {mensagem}\n", formatar_data_utc(agora));

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

/// Formata um instante (epoch em segundos UTC) como "YYYY-MM-DD HH:MM:SS UTC".
///
/// Escrito à mão (zero dependências, sem `chrono`): converte os segundos em data civil
/// usando o algoritmo de calendário do Howard Hinnant (`days_from_civil` ao contrário),
/// que lida com anos bissextos sem tabelas. Código educacional: dá pra ver a conta por baixo.
pub fn formatar_data_utc(epoch_segundos: u64) -> String {
    let segundos_no_dia = epoch_segundos % 86_400;
    let dias_desde_epoch = (epoch_segundos / 86_400) as i64;

    let hora = segundos_no_dia / 3_600;
    let minuto = (segundos_no_dia % 3_600) / 60;
    let segundo = segundos_no_dia % 60;

    let (ano, mes, dia) = data_civil_de_dias(dias_desde_epoch);
    format!("{ano:04}-{mes:02}-{dia:02} {hora:02}:{minuto:02}:{segundo:02} UTC")
}

/// Converte "dias desde 1970-01-01" em (ano, mês, dia). Algoritmo de Howard Hinnant:
/// trata o ano como começando em março para encaixar o 29/02 no fim, evitando casos
/// especiais de bissexto. Válido para qualquer data; aqui sempre receberemos dias >= 0.
fn data_civil_de_dias(dias: i64) -> (i64, u32, u32) {
    // Desloca a origem para 0000-03-01 (era de 400 anos, que repete o padrão bissexto).
    let z = dias + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // dia-da-era: 0..=146096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // ano-da-era: 0..=399
    let ano = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // dia-do-ano (com março como mês 0)
    let mp = (5 * doy + 2) / 153; // mês deslocado: 0..=11 (0 = março)
    let dia = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mes = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // converte de volta para jan..=dez
    let ano = if mes <= 2 { ano + 1 } else { ano };
    (ano, mes, dia)
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn formata_epoch_conhecidos() {
        // Conferidos contra datas UTC conhecidas.
        assert_eq!(formatar_data_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(formatar_data_utc(1_000_000_000), "2001-09-09 01:46:40 UTC");
        // Fim de um ano bissexto (2020): 31/12/2020 23:59:59.
        assert_eq!(formatar_data_utc(1_609_459_199), "2020-12-31 23:59:59 UTC");
        // 29 de fevereiro de 2024 (bissexto) ao meio-dia.
        assert_eq!(formatar_data_utc(1_709_208_000), "2024-02-29 12:00:00 UTC");
    }

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
