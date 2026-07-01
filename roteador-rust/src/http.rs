//! Cliente HTTP/1.1 mínimo escrito à mão sobre `TcpStream` (stdlib pura).
//!
//! Atenção ao escopo: este cliente fala **HTTP simples (sem TLS)**. Serve para o Ollama
//! local (`http://127.0.0.1:11434`), que roda na própria máquina sem criptografia.
//! Provedores remotos (Groq, Gemini) exigem **HTTPS/TLS**, que NÃO implementamos à mão
//! aqui — TLS é complexo demais para reescrever sem dependência. Esses provedores ficam
//! como "slots" tratados em `provedor.rs` até decidirmos a abordagem de TLS (passo futuro).
//!
//! Por que tão baixo nível? O Thiago quer aprender como uma requisição HTTP é, byte a byte:
//! uma linha de pedido, cabeçalhos, linha em branco, corpo. É isto e mais nada.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::erro::FalhaProvedor;

/// Uma URL HTTP já dividida nas partes que precisamos para abrir o socket e montar o pedido.
struct UrlPartes {
    host: String,
    porta: u16,
    caminho: String,
}

/// Resposta HTTP crua: o status numérico e o corpo (texto).
#[derive(Debug, Clone, PartialEq)]
pub struct RespostaHttp {
    pub status: u16,
    pub corpo: String,
}

/// Faz um POST com corpo JSON para uma URL `http://...` e devolve status + corpo.
///
/// `tempo_limite` cobre conexão, escrita e leitura (timeouts de socket). Qualquer erro
/// de rede vira `FalhaProvedor::Rede`, para o roteador cair para o próximo provedor.
pub fn post_json(
    url: &str,
    corpo_json: &str,
    cabecalhos_extra: &[(&str, &str)],
    tempo_limite: Duration,
) -> Result<RespostaHttp, FalhaProvedor> {
    let partes = dividir_url(url)?;

    // Monta a requisição HTTP/1.1 à mão. `Connection: close` simplifica a leitura:
    // o servidor fecha o socket no fim, então sabemos onde o corpo termina.
    let mut requisicao = String::new();
    requisicao.push_str(&format!("POST {} HTTP/1.1\r\n", partes.caminho));
    requisicao.push_str(&format!("Host: {}\r\n", partes.host));
    requisicao.push_str("Connection: close\r\n");
    requisicao.push_str("Content-Type: application/json\r\n");
    for (chave, valor) in cabecalhos_extra {
        requisicao.push_str(&format!("{chave}: {valor}\r\n"));
    }
    requisicao.push_str(&format!("Content-Length: {}\r\n", corpo_json.len()));
    requisicao.push_str("\r\n");
    requisicao.push_str(corpo_json);

    enviar_requisicao(&partes, &requisicao, tempo_limite)
}

/// Faz um GET simples para uma URL `http://...` e devolve status + corpo.
///
/// Serve para consultas BARATAS que não rodam inferência — em especial a checagem de
/// vivacidade do Ollama (`GET /api/tags`, que só lista os modelos instalados). Ver
/// [`crate::diagnostico`]. GET não tem corpo: por isso não mandamos `Content-Length`.
pub fn get(url: &str, tempo_limite: Duration) -> Result<RespostaHttp, FalhaProvedor> {
    let partes = dividir_url(url)?;

    let mut requisicao = String::new();
    requisicao.push_str(&format!("GET {} HTTP/1.1\r\n", partes.caminho));
    requisicao.push_str(&format!("Host: {}\r\n", partes.host));
    requisicao.push_str("Connection: close\r\n");
    requisicao.push_str("\r\n");

    enviar_requisicao(&partes, &requisicao, tempo_limite)
}

/// Abre o socket (com timeout), envia a requisição JÁ MONTADA e lê a resposta inteira.
///
/// Compartilhado por [`post_json`] e [`get`]: o que muda entre eles é só o texto da
/// requisição; a mecânica de rede (conectar, timeouts, escrever, ler até fechar) é a mesma.
fn enviar_requisicao(
    partes: &UrlPartes,
    requisicao: &str,
    tempo_limite: Duration,
) -> Result<RespostaHttp, FalhaProvedor> {
    // Resolve o endereço e conecta com timeout (não trava para sempre se o host sumir).
    let endereco = format!("{}:{}", partes.host, partes.porta);
    let mut enderecos = std::net::ToSocketAddrs::to_socket_addrs(&endereco)
        .map_err(|e| FalhaProvedor::Rede(format!("não resolveu {endereco}: {e}")))?;
    let endereco_socket = enderecos
        .next()
        .ok_or_else(|| FalhaProvedor::Rede(format!("sem endereço para {endereco}")))?;
    let mut conexao = TcpStream::connect_timeout(&endereco_socket, tempo_limite)
        .map_err(|e| FalhaProvedor::Rede(format!("falha ao conectar em {endereco}: {e}")))?;
    conexao
        .set_read_timeout(Some(tempo_limite))
        .map_err(|e| FalhaProvedor::Rede(format!("timeout de leitura: {e}")))?;
    conexao
        .set_write_timeout(Some(tempo_limite))
        .map_err(|e| FalhaProvedor::Rede(format!("timeout de escrita: {e}")))?;

    conexao
        .write_all(requisicao.as_bytes())
        .map_err(|e| FalhaProvedor::Rede(format!("falha ao enviar: {e}")))?;
    conexao
        .flush()
        .map_err(|e| FalhaProvedor::Rede(format!("falha ao dar flush: {e}")))?;

    // Lê a resposta inteira até o servidor fechar a conexão.
    let mut bruto = Vec::new();
    conexao
        .read_to_end(&mut bruto)
        .map_err(|e| FalhaProvedor::Rede(format!("falha ao ler resposta: {e}")))?;

    interpretar_resposta(&bruto)
}

/// Separa a resposta crua em status (primeira linha) e corpo (após a linha em branco).
fn interpretar_resposta(bruto: &[u8]) -> Result<RespostaHttp, FalhaProvedor> {
    // Acha o fim dos cabeçalhos: a sequência \r\n\r\n separa cabeçalhos do corpo.
    let separador = b"\r\n\r\n";
    let posicao_corpo = encontrar_subsequencia(bruto, separador)
        .map(|p| p + separador.len())
        .ok_or_else(|| FalhaProvedor::Rede("resposta HTTP sem separação de cabeçalho".into()))?;

    let cabecalhos = std::str::from_utf8(&bruto[..posicao_corpo])
        .map_err(|_| FalhaProvedor::Rede("cabeçalhos HTTP não-UTF8".into()))?;
    let primeira_linha = cabecalhos
        .lines()
        .next()
        .ok_or_else(|| FalhaProvedor::Rede("resposta HTTP vazia".into()))?;

    // Linha de status: "HTTP/1.1 200 OK" — o número do meio é o status.
    let status = primeira_linha
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| FalhaProvedor::Rede(format!("status HTTP ilegível: '{primeira_linha}'")))?;

    // O corpo pode vir em UTF-8; usamos lossy para nunca quebrar por um byte estranho.
    let corpo = String::from_utf8_lossy(&bruto[posicao_corpo..]).into_owned();
    Ok(RespostaHttp { status, corpo })
}

/// Procura uma subsequência de bytes (ex.: o separador \r\n\r\n) e devolve onde começa.
fn encontrar_subsequencia(palheiro: &[u8], agulha: &[u8]) -> Option<usize> {
    if agulha.is_empty() || palheiro.len() < agulha.len() {
        return None;
    }
    (0..=palheiro.len() - agulha.len()).find(|&i| &palheiro[i..i + agulha.len()] == agulha)
}

/// Divide uma URL `http://host[:porta]/caminho` nas partes necessárias.
fn dividir_url(url: &str) -> Result<UrlPartes, FalhaProvedor> {
    let resto = url.strip_prefix("http://").ok_or_else(|| {
        FalhaProvedor::Rede(format!("este cliente só fala http:// (recebi '{url}')"))
    })?;

    // Separa "host[:porta]" do "/caminho".
    let (autoridade, caminho) = match resto.find('/') {
        Some(pos) => (&resto[..pos], &resto[pos..]),
        None => (resto, "/"),
    };

    // Separa host e porta (porta padrão 80).
    let (host, porta) = match autoridade.rsplit_once(':') {
        Some((h, p)) => {
            let porta = p
                .parse::<u16>()
                .map_err(|_| FalhaProvedor::Rede(format!("porta inválida em '{url}'")))?;
            (h.to_string(), porta)
        }
        None => (autoridade.to_string(), 80),
    };

    if host.is_empty() {
        return Err(FalhaProvedor::Rede(format!("host vazio em '{url}'")));
    }

    Ok(UrlPartes {
        host,
        porta,
        caminho: caminho.to_string(),
    })
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn divide_url_com_porta_e_caminho() {
        let partes = dividir_url("http://127.0.0.1:11434/api/generate").unwrap();
        assert_eq!(partes.host, "127.0.0.1");
        assert_eq!(partes.porta, 11434);
        assert_eq!(partes.caminho, "/api/generate");
    }

    #[test]
    fn divide_url_sem_porta_usa_80_e_barra() {
        let partes = dividir_url("http://exemplo.com").unwrap();
        assert_eq!(partes.host, "exemplo.com");
        assert_eq!(partes.porta, 80);
        assert_eq!(partes.caminho, "/");
    }

    #[test]
    fn recusa_https() {
        assert!(dividir_url("https://api.groq.com/v1").is_err());
    }

    #[test]
    fn interpreta_status_e_corpo() {
        let bruto =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"response\":\"oi\"}";
        let resposta = interpretar_resposta(bruto).unwrap();
        assert_eq!(resposta.status, 200);
        assert_eq!(resposta.corpo, "{\"response\":\"oi\"}");
    }

    #[test]
    fn acha_subsequencia() {
        assert_eq!(encontrar_subsequencia(b"aXYb", b"XY"), Some(1));
        assert_eq!(encontrar_subsequencia(b"abc", b"XY"), None);
    }
}
