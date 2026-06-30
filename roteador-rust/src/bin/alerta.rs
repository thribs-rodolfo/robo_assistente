//! Alerta automático de dependência: avisa o Thiago quando o robô cai no piso (Ollama)
//! N vezes SEGUIDAS — a cadeia de provedores bons está falhando em série.
//!
//! São DOIS alarmes ortogonais sobre o mesmo log, cada um com seu anti-spam:
//!   1. SEQUÊNCIA — quedas no piso SEGUIDAS (cadeia de cima falhando em série AGORA).
//!   2. PERCENTUAL — fração alta de quedas no piso na janela, mesmo sem quedas em série
//!      (cadeia falhando de forma intermitente mas pesada). Ver [`alerta::decidir_por_percentual`].
//!
//! Uso:
//!   alerta                          # log padrão, limites padrão, estados padrão
//!   alerta --limite 3               # alarme de sequência a partir de 3 quedas seguidas
//!   alerta --limiar-percentual 80   # alarme percentual a partir de 80% no piso
//!   alerta --minimo-amostras 12     # percentual só vale com >= 12 roteamentos na janela
//!   alerta --janela 6h              # só considera as últimas 6h do log (aceita 90m, 24h, 7d)
//!   alerta --simular                # decide e imprime, mas NÃO manda Telegram nem grava estado
//!   alerta --estado /tmp/e --estado-percentual /tmp/p --notificador /tmp/n.sh /tmp/log
//!
//! Só LÊ o log (nunca dispara provedor → não toca o Claude, respeita a licao-refresh-token).
//! Quando decide alertar, roda o notificador (default `/root/notificar-thiago.sh`) passando a
//! mensagem como argumento. Anti-spam por arquivo de estado (um por alarme): a sequência avisa
//! de novo só quando piora um degrau ([`alerta::decidir`]); o percentual usa histerese
//! ([`alerta::decidir_por_percentual`]).
//!
//! Saída de processo: 0 normal; 1 em erro de leitura/execução (sem erro silencioso).

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use roteador::alerta;
use roteador::duracao::{descrever_duracao, parsear_duracao};
use roteador::metricas;
use roteador::telemetria;

/// Limite padrão de quedas seguidas no piso antes de alertar. Conservador de propósito:
/// só queremos incomodar o Thiago quando a degradação for clara e em série.
const LIMITE_PADRAO: u64 = 5;
/// Onde guardamos a maior sequência já alertada nesta rajada (anti-spam do alarme de sequência).
const ESTADO_PADRAO: &str = "/var/log/roteador-alerta-piso.estado";
/// Limiar percentual padrão (% de quedas no piso) do alarme percentual. Alto de propósito.
const LIMIAR_PERCENTUAL_PADRAO: u8 = 70;
/// Mínimo de roteamentos na janela para o alarme percentual valer (evita "1 de 1 = 100%").
const MINIMO_AMOSTRAS_PADRAO: u64 = 8;
/// Onde guardamos o liga/desliga "já avisei nesta fase de alta" (anti-spam do alarme percentual).
const ESTADO_PERCENTUAL_PADRAO: &str = "/var/log/roteador-alerta-percentual.estado";
/// Script que manda a mensagem pro Thiago no Telegram.
const NOTIFICADOR_PADRAO: &str = "/root/notificar-thiago.sh";

fn main() -> ExitCode {
    let argumentos: Vec<String> = std::env::args().skip(1).collect();
    let opcoes = match interpretar_argumentos(&argumentos) {
        Ok(opcoes) => opcoes,
        Err(msg) => {
            eprintln!("[alerta] {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 1. Agrega o log (com ou sem janela). Só leitura — não dispara provedor.
    let relatorio = match agregar(&opcoes) {
        Ok(relatorio) => relatorio,
        Err(msg) => {
            eprintln!("[alerta] {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 2. Roda os dois alarmes ortogonais sobre o mesmo relatório. Cada um tem seu próprio
    //    estado anti-spam, então um pode disparar sem o outro. Rodamos ambos mesmo que um
    //    falhe (estados/arquivos independentes) e só no fim decidimos o código de saída —
    //    assim um problema num alarme não esconde o sinal do outro (sem erro silencioso).
    let mut houve_erro = false;
    if let Err(msg) = rodar_alarme_sequencia(&opcoes, &relatorio) {
        eprintln!("[alerta] {msg}");
        houve_erro = true;
    }
    if let Err(msg) = rodar_alarme_percentual(&opcoes, &relatorio) {
        eprintln!("[alerta] {msg}");
        houve_erro = true;
    }
    if houve_erro {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Alarme de SEQUÊNCIA: quedas no piso seguidas (cadeia de cima falhando em série agora).
/// Imprime o raciocínio; em `--simular` mostra o que mandaria sem efeito colateral; senão
/// notifica (quando for o caso) e grava o estado. Se a notificação falhar, NÃO grava o estado,
/// para a próxima rodada tentar de novo em vez de "engolir" o alerta.
fn rodar_alarme_sequencia(opcoes: &Opcoes, relatorio: &metricas::Relatorio) -> Result<(), String> {
    let ja_alertado = ler_estado_tolerante(&opcoes.caminho_estado)?;
    let sequencia = relatorio.sequencia_atual_no_piso;
    let severidade = alerta::severidade(sequencia, opcoes.limite);
    // Decide pelo anti-spam por degraus e, em CRÍTICO, fura o anti-spam (re-avisa a cada rodada).
    let decisao_base = alerta::decidir(sequencia, opcoes.limite, ja_alertado);
    let decisao = alerta::escalonar_por_severidade(decisao_base.clone(), severidade, sequencia);
    let furou_anti_spam = decisao.notificar && !decisao_base.notificar;
    println!(
        "[alerta] seq_no_piso={sequencia} limite={} ja_alertado={ja_alertado} \
         severidade={} -> notificar={} novo_estado={}{}",
        opcoes.limite,
        severidade.etiqueta(),
        decisao.notificar,
        decisao.novo_estado,
        if furou_anti_spam {
            " (CRÍTICO: furou o anti-spam)"
        } else {
            ""
        }
    );

    if opcoes.simular {
        if decisao.notificar {
            println!(
                "[alerta] (simulação) mandaria ao Thiago (sequência):\n{}",
                alerta::mensagem_alerta(relatorio, opcoes.limite)
            );
        }
        return Ok(());
    }

    if decisao.notificar {
        let mensagem = alerta::mensagem_alerta(relatorio, opcoes.limite);
        disparar_notificador(&opcoes.caminho_notificador, &mensagem)?;
        println!("[alerta] Thiago notificado (sequência {sequencia} no piso).");
    }
    gravar_estado(&opcoes.caminho_estado, decisao.novo_estado).map_err(|erro| {
        format!(
            "não consegui gravar estado '{}': {erro}",
            opcoes.caminho_estado
        )
    })
}

/// Alarme PERCENTUAL: fração alta de quedas no piso na janela, mesmo sem quedas em série.
/// Mesma mecânica de impressão/simulação/notificação do alarme de sequência, mas com seu
/// próprio estado (liga/desliga com histerese) gravado como `0`/`1`.
fn rodar_alarme_percentual(opcoes: &Opcoes, relatorio: &metricas::Relatorio) -> Result<(), String> {
    let ja_em_alta = ler_estado_tolerante(&opcoes.caminho_estado_percentual)? != 0;
    let piso = relatorio.sucessos_no_piso();
    let total = relatorio.total_roteamentos();
    // Percentual inteiro (mesma conta da decisão) para classificar a severidade.
    let pct_inteiro = if total == 0 {
        0
    } else {
        (piso as u128 * 100 / total as u128) as u64
    };
    let severidade = alerta::severidade_percentual(pct_inteiro, opcoes.limiar_percentual);
    // Decide pela histerese e, em CRÍTICO, fura a histerese (re-avisa a cada rodada).
    let decisao_base = alerta::decidir_por_percentual(
        piso,
        total,
        opcoes.limiar_percentual,
        opcoes.minimo_amostras,
        ja_em_alta,
    );
    let decisao = alerta::escalonar_percentual_por_severidade(decisao_base.clone(), severidade);
    let furou_anti_spam = decisao.notificar && !decisao_base.notificar;
    println!(
        "[alerta] pct_no_piso={:.0}% ({piso}/{total}) limiar={}% min_amostras={} \
         ja_em_alta={ja_em_alta} severidade={} -> notificar={} novo_estado={}{}",
        relatorio.percentual_no_piso(),
        opcoes.limiar_percentual,
        opcoes.minimo_amostras,
        severidade.etiqueta(),
        decisao.notificar,
        decisao.ja_em_alta,
        if furou_anti_spam {
            " (CRÍTICO: furou a histerese)"
        } else {
            ""
        }
    );

    // Descrição legível da janela para a mensagem (ex.: "24h"); sem janela => "todo o histórico".
    let descricao_janela = opcoes.janela_segundos.map(descrever_duracao);

    if opcoes.simular {
        if decisao.notificar {
            println!(
                "[alerta] (simulação) mandaria ao Thiago (percentual):\n{}",
                alerta::mensagem_alerta_percentual(
                    relatorio,
                    opcoes.limiar_percentual,
                    descricao_janela.as_deref()
                )
            );
        }
        return Ok(());
    }

    if decisao.notificar {
        let mensagem = alerta::mensagem_alerta_percentual(
            relatorio,
            opcoes.limiar_percentual,
            descricao_janela.as_deref(),
        );
        disparar_notificador(&opcoes.caminho_notificador, &mensagem)?;
        println!(
            "[alerta] Thiago notificado (percentual {:.0}% no piso).",
            relatorio.percentual_no_piso()
        );
    }
    // Estado liga/desliga gravado como número (0/1), reaproveitando os helpers de estado u64.
    gravar_estado(
        &opcoes.caminho_estado_percentual,
        if decisao.ja_em_alta { 1 } else { 0 },
    )
    .map_err(|erro| {
        format!(
            "não consegui gravar estado '{}': {erro}",
            opcoes.caminho_estado_percentual
        )
    })
}

/// O que foi pedido na linha de comando.
struct Opcoes {
    /// Caminho do log a ler (default: o da telemetria).
    caminho_log: String,
    /// Janela em segundos, ou `None` para agregar tudo.
    janela_segundos: Option<u64>,
    /// A partir de quantas quedas seguidas no piso alertar (alarme de sequência).
    limite: u64,
    /// A partir de que % de quedas no piso alertar (alarme percentual).
    limiar_percentual: u8,
    /// Mínimo de roteamentos na janela para o alarme percentual valer.
    minimo_amostras: u64,
    /// Arquivo de estado anti-spam do alarme de sequência.
    caminho_estado: String,
    /// Arquivo de estado anti-spam do alarme percentual.
    caminho_estado_percentual: String,
    /// Script notificador a executar quando for alertar.
    caminho_notificador: String,
    /// Se `true`, só decide e imprime — não manda Telegram nem grava estado.
    simular: bool,
}

/// Interpreta os argumentos. Devolve `Err(mensagem)` em uso inválido — sem `panic`.
fn interpretar_argumentos(args: &[String]) -> Result<Opcoes, String> {
    let mut caminho_log: Option<String> = None;
    let mut janela_segundos: Option<u64> = None;
    let mut limite: u64 = LIMITE_PADRAO;
    let mut limiar_percentual: u8 = LIMIAR_PERCENTUAL_PADRAO;
    let mut minimo_amostras: u64 = MINIMO_AMOSTRAS_PADRAO;
    let mut caminho_estado = ESTADO_PADRAO.to_string();
    let mut caminho_estado_percentual = ESTADO_PERCENTUAL_PADRAO.to_string();
    let mut caminho_notificador = NOTIFICADOR_PADRAO.to_string();
    let mut simular = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--limite" || arg == "-l" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um número (ex.: 5)"))?;
            limite = parsear_u64(valor, "limite")?;
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--limite=") {
            limite = parsear_u64(valor, "limite")?;
            i += 1;
        } else if arg == "--limiar-percentual" || arg == "-p" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um número 0–100 (ex.: 70)"))?;
            limiar_percentual = parsear_percentual(valor)?;
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--limiar-percentual=") {
            limiar_percentual = parsear_percentual(valor)?;
            i += 1;
        } else if arg == "--minimo-amostras" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um número (ex.: 8)"))?;
            minimo_amostras = parsear_u64(valor, "minimo-amostras")?;
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--minimo-amostras=") {
            minimo_amostras = parsear_u64(valor, "minimo-amostras")?;
            i += 1;
        } else if arg == "--estado-percentual" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um caminho"))?;
            caminho_estado_percentual = valor.clone();
            i += 2;
        } else if arg == "--janela" || arg == "-j" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um valor (ex.: 24h, 90m, 7d)"))?;
            janela_segundos = Some(parsear_duracao(valor)?);
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--janela=") {
            janela_segundos = Some(parsear_duracao(valor)?);
            i += 1;
        } else if arg == "--estado" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um caminho"))?;
            caminho_estado = valor.clone();
            i += 2;
        } else if arg == "--notificador" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um caminho"))?;
            caminho_notificador = valor.clone();
            i += 2;
        } else if arg == "--simular" || arg == "--dry-run" {
            simular = true;
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("opção desconhecida: {arg}"));
        } else if caminho_log.is_none() {
            caminho_log = Some(arg.clone());
            i += 1;
        } else {
            return Err(format!("argumento extra inesperado: {arg}"));
        }
    }

    Ok(Opcoes {
        caminho_log: caminho_log.unwrap_or_else(|| telemetria::ARQUIVO_LOG.to_string()),
        janela_segundos,
        limite,
        limiar_percentual,
        minimo_amostras,
        caminho_estado,
        caminho_estado_percentual,
        caminho_notificador,
        simular,
    })
}

/// Converte um argumento numérico (`--limite`, `--minimo-amostras`) em `u64`. `rotulo` entra
/// na mensagem de erro para ficar claro qual opção estava errada. `Err` se não for inteiro.
fn parsear_u64(texto: &str, rotulo: &str) -> Result<u64, String> {
    texto
        .trim()
        .parse()
        .map_err(|_| format!("{rotulo} inválido: '{texto}' (use um número, ex.: 5)"))
}

/// Converte o argumento de `--limiar-percentual` em `u8` validando a faixa 0–100.
fn parsear_percentual(texto: &str) -> Result<u8, String> {
    let numero: u64 = texto
        .trim()
        .parse()
        .map_err(|_| format!("percentual inválido: '{texto}' (use um número 0–100, ex.: 70)"))?;
    if numero > 100 {
        return Err(format!("percentual fora da faixa: '{texto}' (use 0–100)"));
    }
    Ok(numero as u8)
}

/// Agrega o log conforme as opções (com janela usa o relógio do sistema).
fn agregar(opcoes: &Opcoes) -> Result<metricas::Relatorio, String> {
    let ler_erro = |erro| format!("não consegui ler '{}': {erro}", opcoes.caminho_log);
    match opcoes.janela_segundos {
        Some(janela) => {
            let agora = agora_epoch().ok_or_else(|| {
                "relógio do sistema antes de 1970 — não dá pra usar janela".to_string()
            })?;
            metricas::agregar_janela_de_arquivo(&opcoes.caminho_log, agora, janela)
                .map_err(ler_erro)
        }
        None => metricas::agregar_de_arquivo(&opcoes.caminho_log).map_err(ler_erro),
    }
}

/// Erros possíveis ao ler o arquivo de estado, separando o recuperável do fatal.
enum EstadoErro {
    /// Conteúdo presente mas não-numérico: recomeça do zero (não fatal).
    Corrompido(String),
    /// Erro de I/O de verdade (permissão, etc.): fatal.
    Io(String),
}

/// Lê o estado anti-spam tolerando corrupção: conteúdo não-numérico só AVISA (não em silêncio)
/// e recomeça do zero; apenas erro de I/O real (permissão, etc.) é fatal e sobe como `Err`.
/// Usado pelos dois alarmes, cada um com seu arquivo de estado.
fn ler_estado_tolerante(caminho: &str) -> Result<u64, String> {
    match ler_estado_do_arquivo(caminho) {
        Ok(valor) => Ok(valor),
        Err(EstadoErro::Corrompido(msg)) => {
            eprintln!("[alerta] {msg}");
            Ok(0)
        }
        Err(EstadoErro::Io(msg)) => Err(msg),
    }
}

/// Lê o estado anterior do arquivo. Arquivo ausente é o caso NORMAL (primeira vez) -> 0.
fn ler_estado_do_arquivo(caminho: &str) -> Result<u64, EstadoErro> {
    match std::fs::read_to_string(caminho) {
        Ok(conteudo) => alerta::parsear_estado(&conteudo).map_err(|erro| {
            EstadoErro::Corrompido(format!(
                "estado corrompido em '{caminho}': {erro} — recomeçando do zero"
            ))
        }),
        Err(erro) if erro.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(erro) => Err(EstadoErro::Io(format!(
            "não consegui ler estado '{caminho}': {erro}"
        ))),
    }
}

/// Grava o novo estado no arquivo (sobrescreve com o número + quebra de linha).
fn gravar_estado(caminho: &str, ja_alertado: u64) -> Result<(), std::io::Error> {
    std::fs::write(caminho, alerta::serializar_estado(ja_alertado))
}

/// Executa o notificador passando a mensagem como argumento. `Err` se não rodar ou sair != 0.
fn disparar_notificador(caminho: &str, mensagem: &str) -> Result<(), String> {
    let status = std::process::Command::new(caminho)
        .arg(mensagem)
        .status()
        .map_err(|erro| format!("não consegui executar o notificador '{caminho}': {erro}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "notificador '{caminho}' saiu com status de erro ({status})"
        ))
    }
}

/// Instante atual em epoch (segundos UTC), ou `None` se o relógio estiver antes de 1970.
fn agora_epoch() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duracao| duracao.as_secs())
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn sem_argumentos_usa_padroes() {
        let o = interpretar_argumentos(&[]).unwrap();
        assert_eq!(o.caminho_log, telemetria::ARQUIVO_LOG);
        assert_eq!(o.janela_segundos, None);
        assert_eq!(o.limite, LIMITE_PADRAO);
        assert_eq!(o.limiar_percentual, LIMIAR_PERCENTUAL_PADRAO);
        assert_eq!(o.minimo_amostras, MINIMO_AMOSTRAS_PADRAO);
        assert_eq!(o.caminho_estado, ESTADO_PADRAO);
        assert_eq!(o.caminho_estado_percentual, ESTADO_PERCENTUAL_PADRAO);
        assert_eq!(o.caminho_notificador, NOTIFICADOR_PADRAO);
        assert!(!o.simular);
    }

    #[test]
    fn interpreta_opcoes_do_alarme_percentual() {
        let o = interpretar_argumentos(&[
            "--limiar-percentual".into(),
            "80".into(),
            "--minimo-amostras".into(),
            "12".into(),
            "--estado-percentual".into(),
            "/tmp/p".into(),
        ])
        .unwrap();
        assert_eq!(o.limiar_percentual, 80);
        assert_eq!(o.minimo_amostras, 12);
        assert_eq!(o.caminho_estado_percentual, "/tmp/p");
        // Forma com '=' também funciona.
        let o2 = interpretar_argumentos(&["--limiar-percentual=65".into()]).unwrap();
        assert_eq!(o2.limiar_percentual, 65);
        // Percentual fora da faixa ou não-numérico vira erro (sem panic).
        assert!(interpretar_argumentos(&["--limiar-percentual".into(), "150".into()]).is_err());
        assert!(interpretar_argumentos(&["--limiar-percentual".into(), "xx".into()]).is_err());
        assert!(interpretar_argumentos(&["--minimo-amostras".into(), "abc".into()]).is_err());
    }

    #[test]
    fn interpreta_opcoes_em_qualquer_ordem() {
        let o = interpretar_argumentos(&[
            "--limite".into(),
            "3".into(),
            "--janela".into(),
            "6h".into(),
            "--simular".into(),
            "/tmp/x.log".into(),
        ])
        .unwrap();
        assert_eq!(o.limite, 3);
        assert_eq!(o.janela_segundos, Some(21_600));
        assert!(o.simular);
        assert_eq!(o.caminho_log, "/tmp/x.log");
    }

    #[test]
    fn aceita_formato_com_igual_e_caminhos_custom() {
        let o = interpretar_argumentos(&[
            "--limite=4".into(),
            "--janela=2d".into(),
            "--estado".into(),
            "/tmp/e".into(),
            "--notificador".into(),
            "/tmp/n.sh".into(),
        ])
        .unwrap();
        assert_eq!(o.limite, 4);
        assert_eq!(o.janela_segundos, Some(172_800));
        assert_eq!(o.caminho_estado, "/tmp/e");
        assert_eq!(o.caminho_notificador, "/tmp/n.sh");
    }

    #[test]
    fn erros_de_uso_viram_err() {
        assert!(interpretar_argumentos(&["--limite".into()]).is_err()); // sem valor
        assert!(interpretar_argumentos(&["--limite".into(), "abc".into()]).is_err()); // não-número
        assert!(interpretar_argumentos(&["--janela".into()]).is_err()); // sem valor
        assert!(interpretar_argumentos(&["--xpto".into()]).is_err()); // opção desconhecida
        assert!(interpretar_argumentos(&["a".into(), "b".into()]).is_err()); // dois logs
    }
}
