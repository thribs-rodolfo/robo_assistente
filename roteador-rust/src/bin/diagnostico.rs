//! Binário que verifica se o PISO (rede de segurança do roteador) está vivo.
//!
//! Uso:
//!   diagnostico                         # sonda o piso da config padrão (/root/.secrets/...)
//!   diagnostico /tmp/outra-config.json  # sonda o piso de outra config
//!
//! Faz uma checagem BARATA (`GET /api/tags` do Ollama — só lista modelos, não roda
//! inferência) e SEGURA (só sonda piso do tipo Ollama; nunca dispara Claude/pagos —
//! licao-refresh-token-rotativo). Útil no cron: se o piso cair, todos os provedores de
//! cima podem cair também e o robô fica mudo — este binário é o alarme desse caso.
//!
//! Código de saída (útil em cron/CI: `diagnostico || avisar`):
//!   0  piso confirmado vivo e com o modelo certo
//!   1  piso comprometido (fora do ar, sem o modelo, desabilitado, inexistente)
//!   2  não deu para verificar (piso não é Ollama → não sondamos)

use std::process::ExitCode;

use roteador::config;
use roteador::diagnostico::{self, Severidade};

fn main() -> ExitCode {
    let argumentos: Vec<String> = std::env::args().skip(1).collect();
    let caminho = match interpretar_argumentos(&argumentos) {
        Ok(caminho) => caminho,
        Err(msg) => {
            eprintln!("[diagnostico] {msg}");
            return ExitCode::FAILURE;
        }
    };

    let config = match config::carregar_de_arquivo(&caminho) {
        Ok(config) => config,
        Err(erro) => {
            eprintln!("[diagnostico] não carreguei '{caminho}': {erro}");
            return ExitCode::FAILURE;
        }
    };

    let resultado = diagnostico::verificar_piso(&config);
    println!("{resultado}");

    // Mapeia a gravidade para o código de saída (0 ok / 1 comprometido / 2 não-verificado).
    match resultado.severidade() {
        Severidade::Ok => ExitCode::SUCCESS,
        Severidade::Comprometido => ExitCode::from(1),
        Severidade::NaoVerificado => ExitCode::from(2),
    }
}

/// Interpreta os argumentos: um caminho opcional (default: [`config::CAMINHO_PADRAO`]).
/// Devolve `Err(mensagem)` em uso inválido — sem `panic`, sem erro silencioso.
fn interpretar_argumentos(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok(config::CAMINHO_PADRAO.to_string()),
        [caminho] if !caminho.starts_with('-') => Ok(caminho.clone()),
        [outro] => Err(format!("opção desconhecida: {outro}")),
        _ => Err("uso: diagnostico [caminho-da-config]".to_string()),
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn sem_argumento_usa_caminho_padrao() {
        assert_eq!(interpretar_argumentos(&[]).unwrap(), config::CAMINHO_PADRAO);
    }

    #[test]
    fn aceita_um_caminho() {
        assert_eq!(
            interpretar_argumentos(&["/tmp/x.json".into()]).unwrap(),
            "/tmp/x.json"
        );
    }

    #[test]
    fn rejeita_opcao_e_argumentos_extras() {
        assert!(interpretar_argumentos(&["--xpto".into()]).is_err());
        assert!(interpretar_argumentos(&["a".into(), "b".into()]).is_err());
    }
}
