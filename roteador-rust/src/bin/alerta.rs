//! Alerta automático de dependência: avisa o Thiago quando o robô cai no piso (Ollama)
//! N vezes SEGUIDAS — a cadeia de provedores bons está falhando em série.
//!
//! Uso:
//!   alerta                          # log padrão, limite 5, estado padrão
//!   alerta --limite 3               # alerta a partir de 3 quedas seguidas no piso
//!   alerta --janela 6h              # só considera as últimas 6h do log (aceita 90m, 24h, 7d)
//!   alerta --simular                # decide e imprime, mas NÃO manda Telegram nem grava estado
//!   alerta --estado /tmp/e --notificador /tmp/n.sh /tmp/log
//!
//! Só LÊ o log (nunca dispara provedor → não toca o Claude, respeita a licao-refresh-token).
//! Quando decide alertar, roda o notificador (default `/root/notificar-thiago.sh`) passando a
//! mensagem como argumento. Anti-spam por arquivo de estado: avisa uma vez por rajada e de novo
//! só quando piora um degrau inteiro (ver [`roteador::alerta::decidir`]).
//!
//! Saída de processo: 0 normal; 1 em erro de leitura/execução (sem erro silencioso).

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use roteador::alerta;
use roteador::duracao::parsear_duracao;
use roteador::metricas;
use roteador::telemetria;

/// Limite padrão de quedas seguidas no piso antes de alertar. Conservador de propósito:
/// só queremos incomodar o Thiago quando a degradação for clara e em série.
const LIMITE_PADRAO: u64 = 5;
/// Onde guardamos a maior sequência já alertada nesta rajada (anti-spam).
const ESTADO_PADRAO: &str = "/var/log/roteador-alerta-piso.estado";
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

    // 2. Lê o estado anterior (maior sequência já alertada nesta rajada).
    let ja_alertado = match ler_estado_do_arquivo(&opcoes.caminho_estado) {
        Ok(valor) => valor,
        // Estado corrompido não é fatal: avisa (não em silêncio) e recomeça do zero.
        Err(EstadoErro::Corrompido(msg)) => {
            eprintln!("[alerta] {msg}");
            0
        }
        // Erro de I/O real (permissão, etc.) é fatal: melhor falhar do que decidir cego.
        Err(EstadoErro::Io(msg)) => {
            eprintln!("[alerta] {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 3. Decide (função pura) e mostra o raciocínio (cron loga isto).
    let sequencia = relatorio.sequencia_atual_no_piso;
    let decisao = alerta::decidir(sequencia, opcoes.limite, ja_alertado);
    println!(
        "[alerta] seq_no_piso={sequencia} limite={} ja_alertado={ja_alertado} \
         -> notificar={} novo_estado={}",
        opcoes.limite, decisao.notificar, decisao.novo_estado
    );

    // 4. Modo simulação: mostra o que faria, sem mandar Telegram nem gravar estado.
    if opcoes.simular {
        if decisao.notificar {
            println!(
                "[alerta] (simulação) mandaria ao Thiago:\n{}",
                alerta::mensagem_alerta(&relatorio, opcoes.limite)
            );
        }
        return ExitCode::SUCCESS;
    }

    // 5. Notifica, se for o caso. Se a notificação falhar, NÃO grava o novo estado —
    //    assim a próxima rodada tenta de novo em vez de "engolir" o alerta.
    if decisao.notificar {
        let mensagem = alerta::mensagem_alerta(&relatorio, opcoes.limite);
        if let Err(msg) = disparar_notificador(&opcoes.caminho_notificador, &mensagem) {
            eprintln!("[alerta] {msg}");
            return ExitCode::FAILURE;
        }
        println!("[alerta] Thiago notificado (sequência {sequencia} no piso).");
    }

    // 6. Grava o novo estado (reset na recuperação ou avanço após alertar). Erro não é silencioso.
    if let Err(erro) = gravar_estado(&opcoes.caminho_estado, decisao.novo_estado) {
        eprintln!(
            "[alerta] não consegui gravar estado '{}': {erro}",
            opcoes.caminho_estado
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// O que foi pedido na linha de comando.
struct Opcoes {
    /// Caminho do log a ler (default: o da telemetria).
    caminho_log: String,
    /// Janela em segundos, ou `None` para agregar tudo.
    janela_segundos: Option<u64>,
    /// A partir de quantas quedas seguidas no piso alertar.
    limite: u64,
    /// Arquivo de estado anti-spam.
    caminho_estado: String,
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
    let mut caminho_estado = ESTADO_PADRAO.to_string();
    let mut caminho_notificador = NOTIFICADOR_PADRAO.to_string();
    let mut simular = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--limite" || arg == "-l" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um número (ex.: 5)"))?;
            limite = parsear_limite(valor)?;
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--limite=") {
            limite = parsear_limite(valor)?;
            i += 1;
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
        caminho_estado,
        caminho_notificador,
        simular,
    })
}

/// Converte o argumento de `--limite` em número. `Err` se não for um inteiro.
fn parsear_limite(texto: &str) -> Result<u64, String> {
    texto
        .trim()
        .parse()
        .map_err(|_| format!("limite inválido: '{texto}' (use um número, ex.: 5)"))
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
        assert_eq!(o.caminho_estado, ESTADO_PADRAO);
        assert_eq!(o.caminho_notificador, NOTIFICADOR_PADRAO);
        assert!(!o.simular);
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
