//! Cliente HTTPS via `curl` (binário externo).
//!
//! Por que `curl` em vez de escrever TLS à mão? TLS é criptografia séria — reescrever
//! sem dependência seria inseguro e gigante. O manifesto permite o **binário externo**
//! como exceção pragmática (é o mesmo princípio do `claude --print`). O `curl` já está
//! na máquina, é onipresente e maduro. Assim mantemos ZERO dependências de *crates* Rust
//! e ainda falamos HTTPS para os provedores remotos (Groq, Gemini) e para o Telegram.
//!
//! Espelha a interface do módulo `http` (POST com corpo JSON -> status + corpo), para os
//! provedores escolherem o transporte certo (http simples local vs. https remoto) sem
//! mudar o resto da lógica.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::erro::FalhaProvedor;
use crate::http::RespostaHttp;

/// Marca que separa o corpo do status na saída do curl (ver `-w` abaixo). É improvável
/// aparecer no corpo de uma resposta JSON, e ainda fazemos o split pela ÚLTIMA ocorrência.
const SEPARADOR_STATUS: &str = "\n__STATUS_HTTP__:";

/// Faz um POST HTTPS com corpo JSON, usando o `curl`. Devolve status + corpo.
///
/// O corpo é enviado pelo stdin do curl (`--data-binary @-`), evitando expor dados grandes
/// (ou chaves dentro do JSON) na linha de comando / lista de processos.
pub fn post_json(
    url: &str,
    corpo_json: &str,
    cabecalhos_extra: &[(&str, &str)],
    tempo_limite: Duration,
) -> Result<RespostaHttp, FalhaProvedor> {
    if !url.starts_with("https://") {
        return Err(FalhaProvedor::Rede(format!(
            "esperava https:// (recebi '{url}')"
        )));
    }

    // Monta os argumentos do curl. `-w` faz o curl imprimir o status DEPOIS do corpo,
    // após o nosso separador, para conseguirmos separar os dois com segurança.
    let segundos = tempo_limite.as_secs().max(1).to_string();
    let formato_status = format!("{SEPARADOR_STATUS}%{{http_code}}");
    let mut comando = Command::new("curl");
    comando
        .arg("-sS") // silencioso, mas mostra erro
        .arg("--max-time")
        .arg(&segundos)
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg("@-") // corpo vem do stdin
        .arg("-w")
        .arg(&formato_status);
    for (chave, valor) in cabecalhos_extra {
        comando.arg("-H").arg(format!("{chave}: {valor}"));
    }
    comando.arg(url);

    let mut filho = comando
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| FalhaProvedor::Rede(format!("não subiu o curl: {e}")))?;

    // Envia o corpo pelo stdin e fecha o pipe.
    {
        let stdin = filho
            .stdin
            .take()
            .ok_or_else(|| FalhaProvedor::Rede("sem stdin no curl".into()))?;
        let mut stdin = stdin;
        stdin
            .write_all(corpo_json.as_bytes())
            .map_err(|e| FalhaProvedor::Rede(format!("falha ao escrever corpo no curl: {e}")))?;
    }

    let saida = filho
        .wait_with_output()
        .map_err(|e| FalhaProvedor::Rede(format!("falha ao aguardar o curl: {e}")))?;

    // curl com erro de transporte (DNS, conexão, timeout) sai com código != 0.
    if !saida.status.success() {
        let erro = String::from_utf8_lossy(&saida.stderr);
        return Err(FalhaProvedor::Rede(format!(
            "curl falhou (código {:?}): {}",
            saida.status.code(),
            erro.trim()
        )));
    }

    let bruto = String::from_utf8_lossy(&saida.stdout).into_owned();
    separar_corpo_e_status(&bruto)
}

/// Separa a saída do curl (corpo + separador + status) em `RespostaHttp`.
fn separar_corpo_e_status(bruto: &str) -> Result<RespostaHttp, FalhaProvedor> {
    // Split pela ÚLTIMA ocorrência do separador: o status é o que vem depois.
    let posicao = bruto
        .rfind(SEPARADOR_STATUS)
        .ok_or_else(|| FalhaProvedor::Rede("saída do curl sem o marcador de status".into()))?;
    let corpo = bruto[..posicao].to_string();
    let status_texto = bruto[posicao + SEPARADOR_STATUS.len()..].trim();
    let status = status_texto
        .parse::<u16>()
        .map_err(|_| FalhaProvedor::Rede(format!("status do curl ilegível: '{status_texto}'")))?;
    Ok(RespostaHttp { status, corpo })
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn separa_corpo_e_status() {
        let bruto = "{\"choices\":[]}\n__STATUS_HTTP__:200";
        let resposta = separar_corpo_e_status(bruto).unwrap();
        assert_eq!(resposta.status, 200);
        assert_eq!(resposta.corpo, "{\"choices\":[]}");
    }

    #[test]
    fn usa_a_ultima_ocorrencia_do_separador() {
        // Mesmo que o corpo contivesse o marcador, o split é pela última ocorrência.
        let bruto = "texto __STATUS_HTTP__:falso\n__STATUS_HTTP__:429";
        let resposta = separar_corpo_e_status(bruto).unwrap();
        assert_eq!(resposta.status, 429);
    }

    #[test]
    fn recusa_url_nao_https() {
        let erro = post_json("http://x/y", "{}", &[], Duration::from_secs(1));
        assert!(matches!(erro, Err(FalhaProvedor::Rede(_))));
    }
}
