//! Conversão de durações legíveis ("24h", "90m", "7d", "3600") <-> segundos.
//!
//! Extraído para ser reaproveitado por mais de um binário (métricas, alerta) sem duplicar
//! o parsing. São funções puras sobre `&str`/`u64` — fáceis de testar e sem efeito colateral.
//!
//! Filosofia (WORKSPACE_RULES "Como escrevemos código"): mínimo de dependências (zero aqui),
//! erro como valor (`Result`, nunca `panic`), código pequeno e legível.

/// Converte "24h", "90m", "7d" ou "3600" (segundos puros) em segundos.
///
/// A última letra, quando presente, é a unidade: `s` segundos, `m` minutos, `h` horas,
/// `d` dias. Sem unidade, o texto inteiro é interpretado como segundos.
/// Devolve `Err(mensagem)` se não casar — sem `panic`, sem erro silencioso.
pub fn parsear_duracao(texto: &str) -> Result<u64, String> {
    let texto = texto.trim();
    if texto.is_empty() {
        return Err("duração vazia".to_string());
    }
    // Último caractere pode ser a unidade (s/m/h/d); sem unidade = segundos.
    let (numero, multiplicador) = match texto.chars().last() {
        Some('s') => (&texto[..texto.len() - 1], 1),
        Some('m') => (&texto[..texto.len() - 1], 60),
        Some('h') => (&texto[..texto.len() - 1], 3_600),
        Some('d') => (&texto[..texto.len() - 1], 86_400),
        _ => (texto, 1),
    };
    let quantidade: u64 = numero
        .parse()
        .map_err(|_| format!("duração inválida: '{texto}' (use ex.: 24h, 90m, 7d, 3600)"))?;
    quantidade
        .checked_mul(multiplicador)
        .ok_or_else(|| format!("duração grande demais: '{texto}'"))
}

/// Descrição curta de uma duração em segundos (para cabeçalhos legíveis).
///
/// Escolhe a maior unidade que divide o valor exatamente: 86400s -> "1d", 21600s -> "6h",
/// 5400s -> "90min", senão segundos.
pub fn descrever_duracao(segundos: u64) -> String {
    if segundos.is_multiple_of(86_400) {
        format!("{}d", segundos / 86_400)
    } else if segundos.is_multiple_of(3_600) {
        format!("{}h", segundos / 3_600)
    } else if segundos.is_multiple_of(60) {
        format!("{}min", segundos / 60)
    } else {
        format!("{segundos}s")
    }
}

/// Descrição APROXIMADA de uma duração em segundos, para frases do tipo "há X".
///
/// Diferente de [`descrever_duracao`] (que só usa a maior unidade que divide EXATAMENTE),
/// aqui a duração quase nunca é redonda (ex.: "há 6902s"), então mostramos as DUAS maiores
/// unidades não-nulas para ficar legível: `1d4h`, `2h3min`, `5min12s`, `45s`. Serve ao
/// relatório de frescor ("último ok do claude há 2h3min").
pub fn descrever_aproximada(segundos: u64) -> String {
    const DIA: u64 = 86_400;
    const HORA: u64 = 3_600;
    const MINUTO: u64 = 60;
    if segundos >= DIA {
        let dias = segundos / DIA;
        let horas = (segundos % DIA) / HORA;
        if horas > 0 {
            format!("{dias}d{horas}h")
        } else {
            format!("{dias}d")
        }
    } else if segundos >= HORA {
        let horas = segundos / HORA;
        let minutos = (segundos % HORA) / MINUTO;
        if minutos > 0 {
            format!("{horas}h{minutos}min")
        } else {
            format!("{horas}h")
        }
    } else if segundos >= MINUTO {
        let minutos = segundos / MINUTO;
        let resto = segundos % MINUTO;
        if resto > 0 {
            format!("{minutos}min{resto}s")
        } else {
            format!("{minutos}min")
        }
    } else {
        format!("{segundos}s")
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn parseia_duracoes_com_unidade() {
        assert_eq!(parsear_duracao("24h"), Ok(86_400));
        assert_eq!(parsear_duracao("90m"), Ok(5_400));
        assert_eq!(parsear_duracao("7d"), Ok(604_800));
        assert_eq!(parsear_duracao("45s"), Ok(45));
        assert_eq!(parsear_duracao("3600"), Ok(3_600)); // sem unidade = segundos
        assert_eq!(parsear_duracao("  12h  "), Ok(43_200)); // espaços nas pontas
    }

    #[test]
    fn rejeita_duracoes_invalidas() {
        assert!(parsear_duracao("").is_err());
        assert!(parsear_duracao("abc").is_err());
        assert!(parsear_duracao("12x").is_err());
    }

    #[test]
    fn descreve_duracao_legivel() {
        assert_eq!(descrever_duracao(86_400), "1d");
        assert_eq!(descrever_duracao(21_600), "6h");
        assert_eq!(descrever_duracao(5_400), "90min");
        assert_eq!(descrever_duracao(45), "45s");
    }

    #[test]
    fn descreve_aproximada_com_duas_unidades() {
        assert_eq!(descrever_aproximada(45), "45s");
        assert_eq!(descrever_aproximada(60), "1min");
        assert_eq!(descrever_aproximada(312), "5min12s");
        assert_eq!(descrever_aproximada(3_600), "1h");
        assert_eq!(descrever_aproximada(7_380), "2h3min");
        assert_eq!(descrever_aproximada(86_400), "1d");
        assert_eq!(descrever_aproximada(100_800), "1d4h");
        assert_eq!(descrever_aproximada(0), "0s");
    }
}
