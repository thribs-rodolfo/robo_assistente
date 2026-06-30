//! Binário que imprime as métricas agregadas de telemetria do roteador.
//!
//! Uso:
//!   metricas                       # lê o log padrão, agrega TUDO
//!   metricas /caminho/outro.log    # lê outro arquivo de log
//!   metricas --janela 24h          # só as últimas 24 horas (aceita 90m, 24h, 7d)
//!   metricas --janela 6h /tmp/x.log
//!
//! Responde, de forma legível, "de quem o robô realmente depende?": por provedor,
//! quantas vezes respondeu/falhou/foi pulado e a latência média; quantas vezes caímos
//! no piso (Ollama); e a sequência de quedas seguidas no piso (alarme de dependência).
//! Só LÊ o log — nunca dispara provedor, então é seguro rodar à vontade.
//!
//! Saída de processo: 0 em sucesso, 1 se não conseguir ler o log ou se o argumento for inválido.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use roteador::duracao::{descrever_duracao, parsear_duracao};
use roteador::metricas;
use roteador::telemetria;

fn main() -> ExitCode {
    // Pula o nome do programa e interpreta os argumentos (ordem livre: --janela e caminho).
    let argumentos: Vec<String> = std::env::args().skip(1).collect();
    let opcoes = match interpretar_argumentos(&argumentos) {
        Ok(opcoes) => opcoes,
        Err(msg) => {
            eprintln!("[metricas] {msg}");
            return ExitCode::FAILURE;
        }
    };

    // Com janela, precisamos do "agora" do relógio do sistema para recortar o passado recente.
    let resultado = match opcoes.janela_segundos {
        Some(janela) => {
            let agora = match agora_epoch() {
                Some(t) => t,
                None => {
                    eprintln!(
                        "[metricas] relógio do sistema antes de 1970 — não dá pra usar janela"
                    );
                    return ExitCode::FAILURE;
                }
            };
            metricas::agregar_janela_de_arquivo(&opcoes.caminho, agora, janela)
        }
        None => metricas::agregar_de_arquivo(&opcoes.caminho),
    };

    match resultado {
        Ok(relatorio) => {
            if let Some(janela) = opcoes.janela_segundos {
                println!("(janela: últimas {})", descrever_duracao(janela));
            }
            print!("{relatorio}");
            ExitCode::SUCCESS
        }
        Err(erro) => {
            eprintln!("[metricas] não consegui ler '{}': {erro}", opcoes.caminho);
            ExitCode::FAILURE
        }
    }
}

/// O que foi pedido na linha de comando.
struct Opcoes {
    /// Caminho do log a ler (default: o da telemetria).
    caminho: String,
    /// Janela em segundos, ou `None` para agregar tudo.
    janela_segundos: Option<u64>,
}

/// Interpreta os argumentos: `--janela <dur>` (em qualquer posição) e, opcionalmente, um caminho.
/// Devolve `Err(mensagem)` em uso inválido — sem `panic`, sem erro silencioso.
fn interpretar_argumentos(args: &[String]) -> Result<Opcoes, String> {
    let mut caminho: Option<String> = None;
    let mut janela_segundos: Option<u64> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--janela" || arg == "-j" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um valor (ex.: 24h, 90m, 7d)"))?;
            janela_segundos = Some(parsear_duracao(valor)?);
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--janela=") {
            janela_segundos = Some(parsear_duracao(valor)?);
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("opção desconhecida: {arg}"));
        } else if caminho.is_none() {
            caminho = Some(arg.clone());
            i += 1;
        } else {
            return Err(format!("argumento extra inesperado: {arg}"));
        }
    }

    Ok(Opcoes {
        caminho: caminho.unwrap_or_else(|| telemetria::ARQUIVO_LOG.to_string()),
        janela_segundos,
    })
}

/// Instante atual em epoch (segundos UTC), ou `None` se o relógio estiver antes de 1970.
fn agora_epoch() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn interpreta_caminho_e_janela_em_qualquer_ordem() {
        let o =
            interpretar_argumentos(&["--janela".into(), "6h".into(), "/tmp/x.log".into()]).unwrap();
        assert_eq!(o.caminho, "/tmp/x.log");
        assert_eq!(o.janela_segundos, Some(21_600));

        let o2 = interpretar_argumentos(&["/tmp/y.log".into(), "--janela=2d".into()]).unwrap();
        assert_eq!(o2.caminho, "/tmp/y.log");
        assert_eq!(o2.janela_segundos, Some(172_800));
    }

    #[test]
    fn sem_argumentos_usa_log_padrao_e_tudo() {
        let o = interpretar_argumentos(&[]).unwrap();
        assert_eq!(o.caminho, telemetria::ARQUIVO_LOG);
        assert_eq!(o.janela_segundos, None);
    }

    #[test]
    fn erros_de_uso_viram_err() {
        assert!(interpretar_argumentos(&["--janela".into()]).is_err()); // sem valor
        assert!(interpretar_argumentos(&["--xpto".into()]).is_err()); // opção desconhecida
        assert!(interpretar_argumentos(&["a".into(), "b".into()]).is_err()); // dois caminhos
    }
}
