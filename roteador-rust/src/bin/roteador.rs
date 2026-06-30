//! Binário de linha de comando para exercitar o roteador.
//!
//! Uso:
//!   roteador "sua mensagem aqui"
//!
//! Lê a config do caminho padrão (`/root/.secrets/roteador-provedores.json`), roteia a
//! mensagem pela cadeia de fallback e imprime a resposta com a telemetria de quem respondeu.
//! Sem argumento, manda uma mensagem-sentinela só para provar que o roteador está de pé.
//!
//! Saída de processo: 0 em sucesso, 1 em erro (todos os provedores falharam ou config ruim).

use std::process::ExitCode;

use roteador::{rotear_com_config_padrao, Contexto};

fn main() -> ExitCode {
    // Junta os argumentos como a mensagem (pulando o nome do binário).
    let mensagem = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let mensagem = if mensagem.trim().is_empty() {
        "Diga apenas: roteador ok.".to_string()
    } else {
        mensagem
    };

    // Contexto vazio por enquanto: o passo 2 (ponte em Rust) vai preencher sistema/histórico.
    match rotear_com_config_padrao(&mensagem, &Contexto::vazio()) {
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
