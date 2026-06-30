//! Métricas agregadas de telemetria: lê o log do roteador e responde
//! "de quem o robô realmente depende?".
//!
//! O módulo [`telemetria`](crate::telemetria) só ANEXA linhas cruas ao log
//! (`/var/log/roteador-provedores.log`). Aqui fazemos o caminho inverso: LEMOS essas
//! linhas e somamos, por provedor, quantas vezes cada um respondeu, falhou ou foi pulado,
//! e a latência média de quem respondeu. O número que mais importa no fim é
//! "quantas vezes caímos no Ollama" — quanto maior, mais o robô está sem provedor bom.
//!
//! Filosofia (WORKSPACE_RULES "Como escrevemos código"): zero dependências (parsing à mão),
//! agnóstico (qualquer nome de provedor entra no mapa), testável (toda a lógica de parsing
//! é função pura sobre `&str`) e sem erro silencioso (a leitura do arquivo devolve `Result`).

use std::collections::BTreeMap;

/// Contagens e latência acumuladas de UM provedor, lidas do log.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MetricasProvedor {
    /// Quantas vezes este provedor respondeu com sucesso (`[ok] respondido por '<nome>'`).
    pub sucessos: u64,
    /// Quantas vezes este provedor foi tentado e falhou (`[falha] <nome>: ...`).
    pub falhas: u64,
    /// Quantas vezes foi pulado na pré-checagem (`[pula] <nome>: ...` — desabilitado/sem chave).
    pub pulos: u64,
    /// Problemas de configuração (na ordem sem config, tipo desconhecido).
    pub problemas_config: u64,
    /// Soma das latências (ms) das respostas com sucesso — para tirar a média depois.
    pub latencia_total_ms: u128,
}

impl MetricasProvedor {
    /// Latência média (ms) das respostas com sucesso, ou `None` se nunca respondeu.
    pub fn latencia_media_ms(&self) -> Option<u128> {
        if self.sucessos == 0 {
            None
        } else {
            Some(self.latencia_total_ms / self.sucessos as u128)
        }
    }
}

/// Relatório agregado de todo o log: um mapa de provedor -> métricas, ordenado por nome.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Relatorio {
    /// Métricas por provedor (BTreeMap = sempre ordenado por nome, saída determinística).
    pub por_provedor: BTreeMap<String, MetricasProvedor>,
    /// Linhas que não casaram com nenhum padrão conhecido (ruído/formato antigo).
    pub linhas_ignoradas: u64,
}

impl Relatorio {
    /// Total de roteamentos concluídos = soma dos sucessos de todos os provedores.
    /// (Cada roteamento termina em exatamente um `[ok]`, enquanto o Ollama segura o piso.)
    pub fn total_roteamentos(&self) -> u64 {
        self.por_provedor.values().map(|m| m.sucessos).sum()
    }

    /// Quantas vezes caímos no provedor-piso (qualquer nome contendo "ollama").
    /// É a métrica-chave de dependência: subir muito = provedores bons estão caindo.
    pub fn sucessos_no_piso(&self) -> u64 {
        self.por_provedor
            .iter()
            .filter(|(nome, _)| nome.to_lowercase().contains("ollama"))
            .map(|(_, m)| m.sucessos)
            .sum()
    }
}

/// Lê o arquivo de log e agrega tudo. Devolve `Err` se não conseguir ler (sem erro silencioso).
pub fn agregar_de_arquivo(caminho: &str) -> Result<Relatorio, std::io::Error> {
    let conteudo = std::fs::read_to_string(caminho)?;
    Ok(agregar(&conteudo))
}

/// Agrega o conteúdo bruto do log (várias linhas) em um [`Relatorio`].
///
/// Função pura: recebe o texto inteiro, devolve as contagens. Toda a lógica de parsing
/// é testável sem tocar em disco.
pub fn agregar(conteudo: &str) -> Relatorio {
    let mut relatorio = Relatorio::default();
    for linha in conteudo.lines() {
        let corpo = match corpo_da_linha(linha) {
            Some(c) => c,
            None => {
                if !linha.trim().is_empty() {
                    relatorio.linhas_ignoradas += 1;
                }
                continue;
            }
        };
        match classificar(corpo) {
            Some(evento) => evento.aplicar(&mut relatorio.por_provedor),
            None => relatorio.linhas_ignoradas += 1,
        }
    }
    relatorio
}

/// Um evento já interpretado de uma linha do log, pronto para somar no mapa.
enum Evento {
    Sucesso { nome: String, latencia_ms: u128 },
    Falha { nome: String },
    Pulo { nome: String },
    ProblemaConfig { nome: String },
}

impl Evento {
    /// Soma este evento nas métricas do provedor correspondente (cria a entrada se faltar).
    fn aplicar(self, por_provedor: &mut BTreeMap<String, MetricasProvedor>) {
        match self {
            Evento::Sucesso { nome, latencia_ms } => {
                let m = por_provedor.entry(nome).or_default();
                m.sucessos += 1;
                m.latencia_total_ms += latencia_ms;
            }
            Evento::Falha { nome } => por_provedor.entry(nome).or_default().falhas += 1,
            Evento::Pulo { nome } => por_provedor.entry(nome).or_default().pulos += 1,
            Evento::ProblemaConfig { nome } => {
                por_provedor.entry(nome).or_default().problemas_config += 1
            }
        }
    }
}

/// Marcador que separa o carimbo de data do corpo da mensagem nas linhas de telemetria.
/// Formato gravado por [`telemetria::registrar_em`](crate::telemetria::registrar_em):
/// `YYYY-MM-DD HH:MM:SS UTC [roteador] <corpo>`.
const MARCADOR: &str = " [roteador] ";

/// Extrai o corpo da mensagem (tudo após `[roteador] `), ou `None` se a linha não casar.
fn corpo_da_linha(linha: &str) -> Option<&str> {
    linha.split_once(MARCADOR).map(|(_data, corpo)| corpo)
}

/// Interpreta o corpo de uma linha em um [`Evento`], ou `None` se for formato desconhecido.
///
/// Os corpos possíveis (ver `lib::rotear` e `telemetria`):
/// - `[ok] respondido por '<nome>' em <N>ms`
/// - `[falha] <nome>: <motivo> (após <N>ms) — caindo pro próximo`
/// - `[pula] <nome>: <motivo>`
/// - `<nome>: na ordem mas sem configuração` / `<nome>: tipo '...' desconhecido`
fn classificar(corpo: &str) -> Option<Evento> {
    if let Some(resto) = corpo.strip_prefix("[ok] respondido por '") {
        let nome = resto.split('\'').next()?.to_string();
        let latencia_ms = extrair_latencia_ms(resto).unwrap_or(0);
        return Some(Evento::Sucesso { nome, latencia_ms });
    }
    if let Some(resto) = corpo.strip_prefix("[falha] ") {
        return Some(Evento::Falha {
            nome: nome_antes_dos_dois_pontos(resto)?,
        });
    }
    if let Some(resto) = corpo.strip_prefix("[pula] ") {
        return Some(Evento::Pulo {
            nome: nome_antes_dos_dois_pontos(resto)?,
        });
    }
    // Problemas de config saem sem prefixo entre colchetes, mas com o padrão `<nome>: ...`.
    if corpo.contains("na ordem mas sem configuração") || corpo.contains("desconhecido") {
        return Some(Evento::ProblemaConfig {
            nome: nome_antes_dos_dois_pontos(corpo)?,
        });
    }
    None
}

/// Pega o nome do provedor antes do primeiro `:` (ex.: `claude: 401 ...` -> `claude`).
/// `None` se não houver `:` ou o nome ficar vazio.
fn nome_antes_dos_dois_pontos(corpo: &str) -> Option<String> {
    let nome = corpo.split(':').next()?.trim();
    if nome.is_empty() {
        None
    } else {
        Some(nome.to_string())
    }
}

/// Acha o número de milissegundos numa frase como `... em 33ms` ou `... (após 1200ms)`.
/// Procura o sufixo `ms` e anda para trás juntando os dígitos coladinhos antes dele.
fn extrair_latencia_ms(texto: &str) -> Option<u128> {
    let posicao_ms = texto.find("ms")?;
    let antes = &texto[..posicao_ms];
    let digitos: String = antes
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digitos.parse().ok()
}

/// Relatório em texto legível, pronto para imprimir no terminal.
///
/// Mostra, por provedor, sucessos/falhas/pulos e latência média; e fecha com a linha
/// que mais importa: quantas vezes (e em que %) o robô caiu no piso (Ollama).
impl std::fmt::Display for Relatorio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "== Métricas do roteador de provedores ==")?;
        let total = self.total_roteamentos();
        if self.por_provedor.is_empty() {
            writeln!(f, "(sem dados no log ainda)")?;
            return Ok(());
        }
        for (nome, m) in &self.por_provedor {
            let latencia = match m.latencia_media_ms() {
                Some(ms) => format!("{ms}ms média"),
                None => "—".to_string(),
            };
            writeln!(
                f,
                "- {nome}: {} ok, {} falha, {} pulo, {} cfg | {latencia}",
                m.sucessos, m.falhas, m.pulos, m.problemas_config
            )?;
        }
        writeln!(f, "total de roteamentos: {total}")?;
        let piso = self.sucessos_no_piso();
        let pct = if total > 0 {
            (piso as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        writeln!(f, "caiu no piso (Ollama): {piso} de {total} ({pct:.1}%)")?;
        if self.linhas_ignoradas > 0 {
            writeln!(f, "(linhas ignoradas: {})", self.linhas_ignoradas)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn extrai_corpo_depois_do_marcador() {
        let linha = "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama' em 33ms";
        assert_eq!(
            corpo_da_linha(linha),
            Some("[ok] respondido por 'ollama' em 33ms")
        );
        assert_eq!(corpo_da_linha("linha sem marcador"), None);
    }

    #[test]
    fn extrai_latencia_em_varios_formatos() {
        assert_eq!(extrair_latencia_ms("em 33ms"), Some(33));
        assert_eq!(extrair_latencia_ms("(após 1200ms) — caindo"), Some(1200));
        assert_eq!(extrair_latencia_ms("sem numero ms"), None);
        assert_eq!(extrair_latencia_ms("sem ms aqui não tem nada"), None);
    }

    #[test]
    fn classifica_sucesso_com_nome_e_latencia() {
        match classificar("[ok] respondido por 'claude' em 850ms") {
            Some(Evento::Sucesso { nome, latencia_ms }) => {
                assert_eq!(nome, "claude");
                assert_eq!(latencia_ms, 850);
            }
            outro => panic!("esperava Sucesso, veio outro: {:?}", outro.is_some()),
        }
    }

    #[test]
    fn classifica_falha_pula_e_config() {
        assert!(matches!(
            classificar("[falha] claude: 401 não autorizado (após 90ms) — caindo pro próximo"),
            Some(Evento::Falha { nome }) if nome == "claude"
        ));
        assert!(matches!(
            classificar("[pula] groq: desabilitado ou sem chave"),
            Some(Evento::Pulo { nome }) if nome == "groq"
        ));
        assert!(matches!(
            classificar("gemini: na ordem mas sem configuração"),
            Some(Evento::ProblemaConfig { nome }) if nome == "gemini"
        ));
        assert!(classificar("linha aleatória qualquer").is_none());
    }

    #[test]
    fn agrega_log_de_exemplo() {
        // Cenário realista: groq pulado e claude falhando algumas vezes, Ollama segurando o piso,
        // e o claude também respondendo em outras. Queremos as contagens e a média certas.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [pula] groq: desabilitado ou sem chave
2026-06-30 12:00:01 UTC [roteador] [falha] claude: 401 (após 90ms) — caindo pro próximo
2026-06-30 12:00:33 UTC [roteador] [ok] respondido por 'ollama_local' em 33000ms
2026-06-30 12:05:00 UTC [roteador] [ok] respondido por 'claude' em 800ms
2026-06-30 12:06:00 UTC [roteador] [ok] respondido por 'claude' em 1200ms
linha de ruído sem formato
";
        let r = agregar(log);

        let groq = &r.por_provedor["groq"];
        assert_eq!(groq.pulos, 1);

        let claude = &r.por_provedor["claude"];
        assert_eq!(claude.sucessos, 2);
        assert_eq!(claude.falhas, 1);
        assert_eq!(claude.latencia_media_ms(), Some(1000)); // (800 + 1200) / 2

        let ollama = &r.por_provedor["ollama_local"];
        assert_eq!(ollama.sucessos, 1);

        assert_eq!(r.total_roteamentos(), 3); // 2 claude + 1 ollama
        assert_eq!(r.sucessos_no_piso(), 1); // só o ollama_local
        assert_eq!(r.linhas_ignoradas, 1); // a linha de ruído
    }

    #[test]
    fn relatorio_vazio_nao_quebra() {
        let r = agregar("");
        assert_eq!(r.total_roteamentos(), 0);
        assert!(format!("{r}").contains("sem dados"));
    }

    #[test]
    fn display_mostra_percentual_do_piso() {
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let texto = format!("{}", agregar(log));
        assert!(texto.contains("total de roteamentos: 2"));
        assert!(texto.contains("caiu no piso (Ollama): 1 de 2 (50.0%)"));
    }
}
