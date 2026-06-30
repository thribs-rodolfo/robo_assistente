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

/// Faz o caminho inverso de [`formatar_data_utc`]: lê "YYYY-MM-DD HH:MM:SS UTC" e
/// devolve o instante em epoch (segundos UTC), ou `None` se o texto não casar com o formato.
///
/// Usado pelas métricas para filtrar o log por janela de tempo (ex.: só as últimas 24h).
/// Validação leve dos campos (mês 1..=12, dia 1..=31, hora/min/seg em faixa) — datas
/// gravadas por nós sempre passam; texto estranho vira `None` em vez de número errado.
pub fn epoch_de_data_utc(texto: &str) -> Option<u64> {
    // O sufixo " UTC" é opcional na entrada, mas é o que a gente sempre grava.
    let texto = texto.trim();
    let texto = texto.strip_suffix(" UTC").unwrap_or(texto);

    // Separa a parte da data ("YYYY-MM-DD") da parte da hora ("HH:MM:SS").
    let (data, hora) = texto.split_once(' ')?;

    let mut campos_data = data.split('-');
    let ano: i64 = campos_data.next()?.parse().ok()?;
    let mes: u32 = campos_data.next()?.parse().ok()?;
    let dia: u32 = campos_data.next()?.parse().ok()?;
    if campos_data.next().is_some() {
        return None; // sobrou campo na data -> formato inválido
    }

    let mut campos_hora = hora.split(':');
    let h: u64 = campos_hora.next()?.parse().ok()?;
    let min: u64 = campos_hora.next()?.parse().ok()?;
    let seg: u64 = campos_hora.next()?.parse().ok()?;
    if campos_hora.next().is_some() {
        return None;
    }

    // Faixas plausíveis (60s tolera leap second eventual). Sem isso, "2026-13-99" passaria.
    if !(1..=12).contains(&mes) || !(1..=31).contains(&dia) || h > 23 || min > 59 || seg > 60 {
        return None;
    }

    let dias = dias_de_data_civil(ano, mes, dia);
    if dias < 0 {
        return None; // antes de 1970: fora do nosso uso (epoch em u64)
    }
    Some(dias as u64 * 86_400 + h * 3_600 + min * 60 + seg)
}

/// Converte (ano, mês, dia) em "dias desde 1970-01-01" — `days_from_civil` de Howard Hinnant,
/// o inverso exato de [`data_civil_de_dias`]. Mesma ideia: ano começa em março pra encaixar
/// o 29/02 no fim e dispensar casos especiais de bissexto.
fn dias_de_data_civil(ano: i64, mes: u32, dia: u32) -> i64 {
    let m = mes as i64;
    let d = dia as i64;
    // Jan/fev contam como meses 13/14 do ano anterior (ano "civil" começa em março).
    let y = if m <= 2 { ano - 1 } else { ano };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // ano-da-era: 0..=399
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // dia-do-ano: 0..=365
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // dia-da-era: 0..=146096
    era * 146_097 + doe - 719_468
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
    fn epoch_de_data_eh_inverso_de_formatar() {
        // Round-trip: formata um epoch e parseia de volta -> tem que voltar o mesmo número.
        for epoch in [
            0_u64,
            1_000_000_000,
            1_609_459_199,
            1_709_208_000,
            1_751_000_000,
        ] {
            let texto = formatar_data_utc(epoch);
            assert_eq!(epoch_de_data_utc(&texto), Some(epoch), "falhou em {texto}");
        }
    }

    #[test]
    fn epoch_de_data_aceita_sem_sufixo_utc() {
        assert_eq!(epoch_de_data_utc("1970-01-01 00:00:00"), Some(0));
        assert_eq!(epoch_de_data_utc("1970-01-01 00:00:00 UTC"), Some(0));
    }

    #[test]
    fn epoch_de_data_rejeita_lixo() {
        assert_eq!(epoch_de_data_utc("não é data"), None);
        assert_eq!(epoch_de_data_utc("2026-13-01 00:00:00 UTC"), None); // mês inválido
        assert_eq!(epoch_de_data_utc("2026-06-30 25:00:00 UTC"), None); // hora inválida
        assert_eq!(epoch_de_data_utc("2026-06-30"), None); // sem hora
        assert_eq!(epoch_de_data_utc("1969-12-31 23:59:59 UTC"), None); // antes do epoch
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
