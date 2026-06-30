//! Binário que imprime as métricas agregadas de telemetria do roteador.
//!
//! Uso:
//!   metricas                      # lê o log padrão (/var/log/roteador-provedores.log)
//!   metricas /caminho/outro.log   # lê outro arquivo de log
//!
//! Responde, de forma legível, "de quem o robô realmente depende?": por provedor,
//! quantas vezes respondeu/falhou/foi pulado e a latência média; e quantas vezes caímos
//! no piso (Ollama). Só LÊ o log — nunca dispara provedor, então é seguro rodar à vontade.
//!
//! Saída de processo: 0 em sucesso, 1 se não conseguir ler o arquivo de log.

use std::process::ExitCode;

use roteador::metricas;
use roteador::telemetria;

fn main() -> ExitCode {
    // Primeiro argumento opcional = caminho do log; senão, o padrão da telemetria.
    let caminho = std::env::args()
        .nth(1)
        .unwrap_or_else(|| telemetria::ARQUIVO_LOG.to_string());

    match metricas::agregar_de_arquivo(&caminho) {
        Ok(relatorio) => {
            print!("{relatorio}");
            ExitCode::SUCCESS
        }
        Err(erro) => {
            eprintln!("[metricas] não consegui ler '{caminho}': {erro}");
            ExitCode::FAILURE
        }
    }
}
