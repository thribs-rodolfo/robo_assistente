//! Binário de linha de comando para exercitar o roteador.
//!
//! Uso:
//!   roteador "sua mensagem aqui"
//!   ROTEADOR_CONFIG=/caminho/outra-config.json roteador "..."
//!
//! Lê a config da env `ROTEADOR_CONFIG` (ou, na ausência dela, do caminho padrão
//! `/root/.secrets/roteador-provedores.json`), roteia a mensagem pela cadeia de fallback e
//! imprime a resposta com a telemetria de quem respondeu. Sem argumento, manda uma
//! mensagem-sentinela só para provar que o roteador está de pé.
//!
//! IMPORTANTE (licao-refresh-token-rotativo): a config padrão começa pelo Claude, então
//! rodar sem `ROTEADOR_CONFIG` DISPARA o Claude. Para exercitar/testar o roteador sem tocar
//! o Claude, aponte `ROTEADOR_CONFIG` para uma config cuja ordem NÃO tenha o Claude (ex.: só
//! Ollama, ou um piso `resposta_fixa`). Espelha o override que a ponte já usa.
//!
//! Saída de processo: 0 em sucesso, 1 em erro (todos os provedores falharam ou config ruim).

use std::process::ExitCode;

use roteador::config::{carregar_de_arquivo, CAMINHO_PADRAO};
use roteador::{rotear, Contexto};

/// Caminho da config do roteador: env `ROTEADOR_CONFIG` ou o padrão (em `/root/.secrets/`).
/// Idêntico ao override que o binário da ponte usa — assim dá para testar uma config
/// alternativa sem editar o arquivo de produção (nem disparar o Claude sem querer).
fn caminho_config() -> String {
    std::env::var("ROTEADOR_CONFIG").unwrap_or_else(|_| CAMINHO_PADRAO.to_string())
}

fn main() -> ExitCode {
    // Junta os argumentos como a mensagem (pulando o nome do binário).
    let mensagem = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let mensagem = if mensagem.trim().is_empty() {
        "Diga apenas: roteador ok.".to_string()
    } else {
        mensagem
    };

    // Carrega a config (com override por env) e roteia. Contexto vazio por enquanto.
    let config = match carregar_de_arquivo(&caminho_config()) {
        Ok(c) => c,
        Err(erro) => {
            eprintln!("[roteador] não carreguei a config: {erro}");
            return ExitCode::FAILURE;
        }
    };
    match rotear(&mensagem, &Contexto::vazio(), &config) {
        Ok(resposta) => {
            println!("[provedor: {}]\n{}", resposta.provedor, resposta.texto);
            ExitCode::SUCCESS
        }
        Err(erro) => {
            eprintln!("[roteador] erro: {erro}");
            ExitCode::FAILURE
        }
    }
}
