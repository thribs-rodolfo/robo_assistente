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
    /// Quantos dos roteamentos MAIS RECENTES, em sequência, caíram no piso (Ollama).
    /// Zera assim que um provedor bom responde. É o alarme de "estou sem provedor bom AGORA":
    /// se está alto, a cadeia toda de cima vem falhando em série.
    pub sequencia_atual_no_piso: u64,
    /// A maior sequência de roteamentos seguidos no piso já vista no período analisado.
    pub maior_sequencia_no_piso: u64,
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
            .filter(|(nome, _)| eh_piso(nome))
            .map(|(_, m)| m.sucessos)
            .sum()
    }

    /// Atualiza as sequências de "caiu no piso" a cada `[ok]`, na ordem cronológica do log.
    /// Cada resposta de provedor bom zera a sequência atual; cada Ollama soma +1.
    fn registrar_sequencia(&mut self, nome: &str) {
        if eh_piso(nome) {
            self.sequencia_atual_no_piso += 1;
            if self.sequencia_atual_no_piso > self.maior_sequencia_no_piso {
                self.maior_sequencia_no_piso = self.sequencia_atual_no_piso;
            }
        } else {
            self.sequencia_atual_no_piso = 0;
        }
    }
}

/// Um provedor é o "piso" se o nome contém "ollama" (case-insensitive). Único ponto de decisão.
fn eh_piso(nome: &str) -> bool {
    nome.to_lowercase().contains("ollama")
}

/// Lê o arquivo de log e agrega tudo. Devolve `Err` se não conseguir ler (sem erro silencioso).
pub fn agregar_de_arquivo(caminho: &str) -> Result<Relatorio, std::io::Error> {
    let conteudo = std::fs::read_to_string(caminho)?;
    Ok(agregar(&conteudo))
}

/// Igual a [`agregar_de_arquivo`], mas só conta o que aconteceu nos últimos
/// `janela_segundos` antes de `agora_epoch` (ex.: 24h). Linhas sem timestamp ou fora
/// da janela são simplesmente ignoradas na soma (não contam como ruído).
pub fn agregar_janela_de_arquivo(
    caminho: &str,
    agora_epoch: u64,
    janela_segundos: u64,
) -> Result<Relatorio, std::io::Error> {
    let conteudo = std::fs::read_to_string(caminho)?;
    Ok(agregar_janela(&conteudo, agora_epoch, janela_segundos))
}

/// Agrega o conteúdo bruto do log (várias linhas) em um [`Relatorio`] — tudo, sem recorte.
///
/// Função pura: recebe o texto inteiro, devolve as contagens. Toda a lógica de parsing
/// é testável sem tocar em disco.
pub fn agregar(conteudo: &str) -> Relatorio {
    agregar_interno(conteudo, None)
}

/// Agrega só as linhas dentro da janela `[agora_epoch - janela_segundos, agora_epoch]`.
///
/// Útil para responder "nas últimas 24h, de quem o robô dependeu?" sem o peso do histórico
/// inteiro. Linhas anteriores à janela (ou sem timestamp legível) ficam de fora da soma.
pub fn agregar_janela(conteudo: &str, agora_epoch: u64, janela_segundos: u64) -> Relatorio {
    let inicio = agora_epoch.saturating_sub(janela_segundos);
    agregar_interno(conteudo, Some(inicio..=agora_epoch))
}

/// Núcleo compartilhado: percorre as linhas em ordem e soma os eventos. Se `janela` for
/// `Some(faixa)`, só conta linhas cujo timestamp está dentro da faixa (epoch UTC).
fn agregar_interno(conteudo: &str, janela: Option<std::ops::RangeInclusive<u64>>) -> Relatorio {
    let mut relatorio = Relatorio::default();
    for linha in conteudo.lines() {
        let (carimbo, corpo) = match separar_linha(linha) {
            Some(par) => par,
            None => {
                if !linha.trim().is_empty() {
                    relatorio.linhas_ignoradas += 1;
                }
                continue;
            }
        };
        // Recorte por janela: sem timestamp legível ou fora da faixa -> não entra na soma.
        if let Some(faixa) = &janela {
            match carimbo {
                Some(instante) if faixa.contains(&instante) => {}
                _ => continue,
            }
        }
        match classificar(corpo) {
            Some(evento) => {
                // A sequência de piso só faz sentido para roteamentos concluídos (`[ok]`).
                if let Evento::Sucesso { nome, .. } = &evento {
                    relatorio.registrar_sequencia(nome);
                }
                evento.aplicar(&mut relatorio.por_provedor);
            }
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

/// Separa a linha em (instante, corpo): o carimbo vira epoch UTC (ou `None` se ilegível)
/// e o corpo é tudo após `[roteador] `. Devolve `None` só quando o marcador nem existe.
fn separar_linha(linha: &str) -> Option<(Option<u64>, &str)> {
    let (data, corpo) = linha.split_once(MARCADOR)?;
    Some((crate::telemetria::epoch_de_data_utc(data), corpo))
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
        // Sequências de piso: alarme de dependência AGORA (atual) e pior momento (máxima).
        writeln!(
            f,
            "sequência no piso: {} agora (máx. {})",
            self.sequencia_atual_no_piso, self.maior_sequencia_no_piso
        )?;
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
    fn separa_carimbo_e_corpo_depois_do_marcador() {
        let linha = "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama' em 33ms";
        let (carimbo, corpo) = separar_linha(linha).unwrap();
        assert_eq!(corpo, "[ok] respondido por 'ollama' em 33ms");
        // O carimbo vira epoch (mesmo instante que a telemetria gravaria).
        assert_eq!(
            carimbo,
            crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC")
        );
        // Sem o marcador, nem dá pra separar.
        assert_eq!(separar_linha("linha sem marcador"), None);
        // Com marcador mas data corrompida: separa o corpo, mas o carimbo fica None.
        let (sem_data, corpo2) = separar_linha("lixo [roteador] [pula] groq: x").unwrap();
        assert_eq!(sem_data, None);
        assert_eq!(corpo2, "[pula] groq: x");
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
    fn conta_sequencia_consecutiva_no_piso() {
        // Ordem cronológica: bom, piso, piso, piso, bom, piso, piso.
        // Maior sequência seguida = 3; a atual (no fim) = 2.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 31000ms
2026-06-30 12:03:00 UTC [roteador] [ok] respondido por 'ollama_local' em 32000ms
2026-06-30 12:04:00 UTC [roteador] [ok] respondido por 'claude' em 600ms
2026-06-30 12:05:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:06:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let r = agregar(log);
        assert_eq!(r.maior_sequencia_no_piso, 3);
        assert_eq!(r.sequencia_atual_no_piso, 2);
        assert!(format!("{r}").contains("sequência no piso: 2 agora (máx. 3)"));
    }

    #[test]
    fn janela_de_tempo_recorta_o_log() {
        // Duas linhas: uma velha (fora de 1h) e uma recente (dentro). Só a recente conta.
        let velha = crate::telemetria::epoch_de_data_utc("2026-06-30 10:00:00 UTC").unwrap();
        let recente = crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC").unwrap();
        let agora = recente; // "agora" = instante da linha recente
        let log = "\
2026-06-30 10:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        // Janela de 1h: deixa de fora a linha das 10:00.
        let r = agregar_janela(log, agora, 3_600);
        assert!(!r.por_provedor.contains_key("claude"));
        assert_eq!(r.por_provedor["ollama_local"].sucessos, 1);
        assert_eq!(r.total_roteamentos(), 1);

        // Janela larga (3h): pega as duas. Confirma que o recorte é o que muda.
        let r3h = agregar_janela(log, agora, 3 * 3_600);
        assert_eq!(r3h.total_roteamentos(), 2);
        let _ = velha; // documentado: linha velha existe, só foi recortada na janela de 1h
    }

    #[test]
    fn linha_sem_timestamp_legivel_fica_fora_da_janela() {
        // Timestamp corrompido -> não dá pra situar no tempo -> não entra na visão por janela.
        let log = "data-quebrada [roteador] [ok] respondido por 'claude' em 500ms\n";
        let r = agregar_janela(log, 2_000_000_000, 86_400);
        assert_eq!(r.total_roteamentos(), 0);
        // Mas no agregado completo (sem janela) ela conta normalmente.
        assert_eq!(agregar(log).total_roteamentos(), 1);
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
