//! Servidor HTTP/1.1 mínimo escrito à mão sobre `TcpStream` (stdlib pura).
//!
//! É o lado SERVIDOR da ponte: o Telegram (via nginx, que termina o HTTPS) entrega os
//! webhooks aqui em `http://127.0.0.1:18800`. Como o nginx já cuidou do TLS, aqui só
//! precisamos de HTTP simples — nada de criptografia à mão.
//!
//! Por que tão baixo nível? O Thiago quer aprender como um pedido HTTP é, byte a byte:
//! uma linha de pedido (`POST /caminho HTTP/1.1`), cabeçalhos, linha em branco, corpo.
//! É isto e mais nada. Mantemos o parsing separado do socket para conseguir testá-lo.

use std::io::{BufRead, BufReader, Read, Write};

/// Um pedido HTTP já parseado: método, caminho, cabeçalhos e corpo.
#[derive(Debug, Clone, PartialEq)]
pub struct Requisicao {
    /// Verbo HTTP em maiúsculas (ex.: "GET", "POST").
    pub metodo: String,
    /// Caminho pedido, sem o host (ex.: "/ponte-telegram/ronaldo").
    pub caminho: String,
    /// Cabeçalhos como pares (nome, valor). O nome é guardado como veio na linha.
    pub cabecalhos: Vec<(String, String)>,
    /// Corpo da requisição (texto). Vazio quando não há `Content-Length`.
    pub corpo: String,
}

impl Requisicao {
    /// Busca um cabeçalho pelo nome, sem diferenciar maiúsculas/minúsculas
    /// (HTTP trata "Content-Length" e "content-length" como iguais).
    pub fn cabecalho(&self, nome: &str) -> Option<&str> {
        self.cabecalhos
            .iter()
            .find(|(chave, _)| chave.eq_ignore_ascii_case(nome))
            .map(|(_, valor)| valor.as_str())
    }
}

/// Erro ao ler/parsear uma requisição HTTP. Valor tipado, nunca pânico.
#[derive(Debug, Clone, PartialEq)]
pub enum ErroHttp {
    /// O fluxo terminou antes de uma requisição completa (cliente desconectou).
    FimInesperado,
    /// A linha de pedido (primeira linha) está malformada.
    LinhaInvalida(String),
    /// Falha de E/S ao ler do socket.
    Leitura(String),
}

impl std::fmt::Display for ErroHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErroHttp::FimInesperado => write!(f, "fim inesperado da requisição"),
            ErroHttp::LinhaInvalida(linha) => write!(f, "linha de pedido inválida: {linha}"),
            ErroHttp::Leitura(motivo) => write!(f, "erro de leitura: {motivo}"),
        }
    }
}

impl std::error::Error for ErroHttp {}

/// Lê e parseia uma requisição HTTP a partir de qualquer fonte de bytes (`Read`).
///
/// Receber um `Read` genérico (em vez do `TcpStream` direto) é o que torna isto testável:
/// nos testes passamos um `Cursor` com bytes fixos; em produção, o socket.
pub fn ler_requisicao<L: Read>(fonte: L) -> Result<Requisicao, ErroHttp> {
    let mut leitor = BufReader::new(fonte);

    // 1) Linha de pedido: "MÉTODO CAMINHO HTTP/versão".
    let primeira = ler_linha(&mut leitor)?;
    if primeira.is_empty() {
        return Err(ErroHttp::FimInesperado);
    }
    let mut campos = primeira.split_whitespace();
    let metodo = campos
        .next()
        .ok_or_else(|| ErroHttp::LinhaInvalida(primeira.clone()))?
        .to_string();
    let caminho = campos
        .next()
        .ok_or_else(|| ErroHttp::LinhaInvalida(primeira.clone()))?
        .to_string();

    // 2) Cabeçalhos: "Nome: valor" por linha, até uma linha em branco.
    let mut cabecalhos = Vec::new();
    loop {
        let linha = ler_linha(&mut leitor)?;
        if linha.is_empty() {
            break; // linha em branco separa cabeçalhos do corpo
        }
        if let Some((nome, valor)) = linha.split_once(':') {
            cabecalhos.push((nome.trim().to_string(), valor.trim().to_string()));
        }
        // Linha sem ':' é ignorada (tolerante a ruído), não derruba o parsing.
    }

    // 3) Corpo: lê exatamente `Content-Length` bytes, se houver.
    let tamanho = cabecalhos
        .iter()
        .find(|(chave, _)| chave.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, valor)| valor.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let corpo = if tamanho > 0 {
        let mut buffer = vec![0u8; tamanho];
        leitor
            .read_exact(&mut buffer)
            .map_err(|e| ErroHttp::Leitura(e.to_string()))?;
        String::from_utf8_lossy(&buffer).into_owned()
    } else {
        String::new()
    };

    Ok(Requisicao {
        metodo,
        caminho,
        cabecalhos,
        corpo,
    })
}

/// Lê uma linha terminada em `\r\n` (ou `\n`), devolvendo-a SEM o terminador.
/// Uma linha vazia (só o terminador) devolve `""`, o que sinaliza fim dos cabeçalhos.
fn ler_linha<L: BufRead>(leitor: &mut L) -> Result<String, ErroHttp> {
    let mut bruto = Vec::new();
    let lidos = leitor
        .read_until(b'\n', &mut bruto)
        .map_err(|e| ErroHttp::Leitura(e.to_string()))?;
    if lidos == 0 {
        // EOF sem nada lido. Devolvemos linha vazia para o chamador decidir o que fazer.
        return Ok(String::new());
    }
    // Remove o '\n' final e um '\r' anterior, se houver (terminador CRLF do HTTP).
    while matches!(bruto.last(), Some(b'\n') | Some(b'\r')) {
        bruto.pop();
    }
    Ok(String::from_utf8_lossy(&bruto).into_owned())
}

/// Uma resposta HTTP a ser escrita de volta no socket.
#[derive(Debug, Clone, PartialEq)]
pub struct Resposta {
    /// Código de status (ex.: 200, 403, 404).
    pub status: u16,
    /// Frase curta do status (ex.: "OK", "Forbidden").
    pub frase: String,
    /// Valor do cabeçalho `Content-Type`.
    pub tipo_conteudo: String,
    /// Corpo da resposta.
    pub corpo: String,
}

impl Resposta {
    /// Resposta de texto simples (`text/plain`).
    pub fn texto(status: u16, corpo: &str) -> Self {
        Resposta {
            status,
            frase: frase_padrao(status).to_string(),
            tipo_conteudo: "text/plain; charset=utf-8".to_string(),
            corpo: corpo.to_string(),
        }
    }

    /// Resposta com corpo JSON (`application/json`).
    pub fn json(status: u16, corpo: &str) -> Self {
        Resposta {
            status,
            frase: frase_padrao(status).to_string(),
            tipo_conteudo: "application/json".to_string(),
            corpo: corpo.to_string(),
        }
    }

    /// Serializa a resposta no formato HTTP/1.1 cru (linha de status + cabeçalhos + corpo).
    pub fn serializar(&self) -> Vec<u8> {
        let corpo_bytes = self.corpo.as_bytes();
        let cabecalho = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.status,
            self.frase,
            self.tipo_conteudo,
            corpo_bytes.len()
        );
        let mut saida = cabecalho.into_bytes();
        saida.extend_from_slice(corpo_bytes);
        saida
    }

    /// Escreve a resposta inteira no destino (socket) e força o envio (`flush`).
    pub fn escrever_em<E: Write>(&self, mut destino: E) -> std::io::Result<()> {
        destino.write_all(&self.serializar())?;
        destino.flush()
    }
}

/// Frase padrão para os poucos status que usamos. Genérica para o resto.
fn frase_padrao(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

#[cfg(test)]
mod testes {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parseia_post_com_corpo() {
        let bruto = "POST /ponte-telegram/ronaldo HTTP/1.1\r\n\
                     Host: localhost\r\n\
                     Content-Length: 13\r\n\
                     X-Telegram-Bot-Api-Secret-Token: segredo\r\n\
                     \r\n\
                     {\"ok\":\"sim\"}\n";
        let req = ler_requisicao(Cursor::new(bruto)).unwrap();
        assert_eq!(req.metodo, "POST");
        assert_eq!(req.caminho, "/ponte-telegram/ronaldo");
        assert_eq!(req.cabecalho("content-length"), Some("13"));
        assert_eq!(
            req.cabecalho("X-Telegram-Bot-Api-Secret-Token"),
            Some("segredo")
        );
        assert_eq!(req.corpo, "{\"ok\":\"sim\"}\n");
    }

    #[test]
    fn parseia_get_sem_corpo() {
        let bruto = "GET /ponte-telegram/saude HTTP/1.1\r\nHost: x\r\n\r\n";
        let req = ler_requisicao(Cursor::new(bruto)).unwrap();
        assert_eq!(req.metodo, "GET");
        assert_eq!(req.caminho, "/ponte-telegram/saude");
        assert_eq!(req.corpo, "");
    }

    #[test]
    fn cabecalho_e_insensivel_a_caixa() {
        let bruto = "GET / HTTP/1.1\r\nContent-Type: application/json\r\n\r\n";
        let req = ler_requisicao(Cursor::new(bruto)).unwrap();
        assert_eq!(req.cabecalho("CONTENT-TYPE"), Some("application/json"));
    }

    #[test]
    fn fluxo_vazio_da_fim_inesperado() {
        let erro = ler_requisicao(Cursor::new("")).unwrap_err();
        assert_eq!(erro, ErroHttp::FimInesperado);
    }

    #[test]
    fn serializa_resposta_json() {
        let resposta = Resposta::json(200, "{\"ok\":true}");
        let bytes = resposta.serializar();
        let texto = String::from_utf8_lossy(&bytes);
        assert!(texto.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(texto.contains("Content-Type: application/json\r\n"));
        assert!(texto.contains("Content-Length: 11\r\n"));
        assert!(texto.ends_with("\r\n\r\n{\"ok\":true}"));
    }
}
