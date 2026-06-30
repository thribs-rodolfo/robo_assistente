//! JSON mínimo escrito à mão (sem dependências).
//!
//! Por que escrever nós mesmos? O manifesto do workspace pede o mínimo de dependências
//! e código educacional. Um JSON é simples o bastante para implementarmos um parser de
//! descida recursiva e um codificador em poucas linhas — e assim o Thiago vê como funciona
//! por baixo, em vez de "mágica" de uma crate.
//!
//! Suportamos o subconjunto de JSON que o roteador precisa: objetos, listas, textos,
//! números, booleanos e nulo. Cobre tanto a config local quanto as respostas dos provedores.

use std::collections::BTreeMap;
use std::fmt;

/// Um valor JSON já parseado. Mantemos objetos como lista de pares para preservar a
/// ordem de inserção (útil ao montar requisições previsíveis), mas oferecemos busca por chave.
#[derive(Debug, Clone, PartialEq)]
pub enum Valor {
    Nulo,
    Booleano(bool),
    Numero(f64),
    Texto(String),
    Lista(Vec<Valor>),
    Objeto(Vec<(String, Valor)>),
}

/// Erro de parsing: posição (byte) onde falhou + motivo. Erro é valor tipado, nunca pânico.
#[derive(Debug, Clone, PartialEq)]
pub struct ErroJson {
    pub posicao: usize,
    pub motivo: String,
}

impl fmt::Display for ErroJson {
    fn fmt(&self, formatador: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatador,
            "JSON inválido na posição {}: {}",
            self.posicao, self.motivo
        )
    }
}

impl std::error::Error for ErroJson {}

impl Valor {
    /// Busca uma chave em um objeto. Devolve `None` se não for objeto ou a chave não existir.
    pub fn obter(&self, chave: &str) -> Option<&Valor> {
        match self {
            Valor::Objeto(pares) => pares.iter().find(|(k, _)| k == chave).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Acessa um índice de uma lista. `None` se não for lista ou estiver fora do intervalo.
    pub fn indice(&self, posicao: usize) -> Option<&Valor> {
        match self {
            Valor::Lista(itens) => itens.get(posicao),
            _ => None,
        }
    }

    /// Devolve o texto, se este valor for um `Texto`.
    pub fn como_texto(&self) -> Option<&str> {
        match self {
            Valor::Texto(s) => Some(s),
            _ => None,
        }
    }

    /// Devolve o booleano, se este valor for um `Booleano`.
    pub fn como_booleano(&self) -> Option<bool> {
        match self {
            Valor::Booleano(b) => Some(*b),
            _ => None,
        }
    }

    /// Devolve o número, se este valor for um `Numero`.
    pub fn como_numero(&self) -> Option<f64> {
        match self {
            Valor::Numero(n) => Some(*n),
            _ => None,
        }
    }

    /// Devolve a lista, se este valor for uma `Lista`.
    pub fn como_lista(&self) -> Option<&[Valor]> {
        match self {
            Valor::Lista(itens) => Some(itens),
            _ => None,
        }
    }

    /// Serializa este valor de volta para texto JSON compacto.
    pub fn para_texto(&self) -> String {
        let mut saida = String::new();
        self.escrever_em(&mut saida);
        saida
    }

    fn escrever_em(&self, saida: &mut String) {
        match self {
            Valor::Nulo => saida.push_str("null"),
            Valor::Booleano(true) => saida.push_str("true"),
            Valor::Booleano(false) => saida.push_str("false"),
            Valor::Numero(n) => saida.push_str(&formatar_numero(*n)),
            Valor::Texto(s) => escrever_texto_json(s, saida),
            Valor::Lista(itens) => {
                saida.push('[');
                for (i, item) in itens.iter().enumerate() {
                    if i > 0 {
                        saida.push(',');
                    }
                    item.escrever_em(saida);
                }
                saida.push(']');
            }
            Valor::Objeto(pares) => {
                saida.push('{');
                for (i, (chave, valor)) in pares.iter().enumerate() {
                    if i > 0 {
                        saida.push(',');
                    }
                    escrever_texto_json(chave, saida);
                    saida.push(':');
                    valor.escrever_em(saida);
                }
                saida.push('}');
            }
        }
    }
}

/// Conveniência para montar objetos JSON sem repetir `to_string()`.
/// Recebe pares (chave, Valor) e devolve um `Valor::Objeto`.
pub fn objeto(pares: BTreeMap<String, Valor>) -> Valor {
    Valor::Objeto(pares.into_iter().collect())
}

/// Formata um número evitando notação científica e o `.0` desnecessário em inteiros.
fn formatar_numero(numero: f64) -> String {
    if numero.fract() == 0.0 && numero.is_finite() && numero.abs() < 1e15 {
        format!("{}", numero as i64)
    } else {
        format!("{numero}")
    }
}

/// Escreve um texto com as aspas e os escapes que o JSON exige.
fn escrever_texto_json(texto: &str, saida: &mut String) {
    saida.push('"');
    for caractere in texto.chars() {
        match caractere {
            '"' => saida.push_str("\\\""),
            '\\' => saida.push_str("\\\\"),
            '\n' => saida.push_str("\\n"),
            '\r' => saida.push_str("\\r"),
            '\t' => saida.push_str("\\t"),
            c if (c as u32) < 0x20 => saida.push_str(&format!("\\u{:04x}", c as u32)),
            c => saida.push(c),
        }
    }
    saida.push('"');
}

/// Parser de descida recursiva. Caminha pelos bytes mantendo a posição atual.
struct Analisador<'a> {
    bytes: &'a [u8],
    posicao: usize,
}

/// Ponto de entrada: parseia um texto JSON completo para um `Valor`.
pub fn parsear(texto: &str) -> Result<Valor, ErroJson> {
    let mut analisador = Analisador {
        bytes: texto.as_bytes(),
        posicao: 0,
    };
    analisador.pular_espacos();
    let valor = analisador.valor()?;
    analisador.pular_espacos();
    if analisador.posicao != analisador.bytes.len() {
        return Err(analisador.erro("sobrou conteúdo após o valor JSON"));
    }
    Ok(valor)
}

impl<'a> Analisador<'a> {
    fn erro(&self, motivo: &str) -> ErroJson {
        ErroJson {
            posicao: self.posicao,
            motivo: motivo.to_string(),
        }
    }

    fn byte_atual(&self) -> Option<u8> {
        self.bytes.get(self.posicao).copied()
    }

    fn pular_espacos(&mut self) {
        while let Some(b) = self.byte_atual() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.posicao += 1;
            } else {
                break;
            }
        }
    }

    /// Parseia qualquer valor JSON a partir da posição atual.
    fn valor(&mut self) -> Result<Valor, ErroJson> {
        self.pular_espacos();
        match self.byte_atual() {
            Some(b'{') => self.objeto(),
            Some(b'[') => self.lista(),
            Some(b'"') => Ok(Valor::Texto(self.texto()?)),
            Some(b't') | Some(b'f') => self.booleano(),
            Some(b'n') => self.nulo(),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.numero(),
            Some(_) => Err(self.erro("caractere inesperado no início de um valor")),
            None => Err(self.erro("fim inesperado do JSON")),
        }
    }

    fn consumir_literal(&mut self, literal: &str, valor: Valor) -> Result<Valor, ErroJson> {
        let bytes = literal.as_bytes();
        if self.bytes[self.posicao..].starts_with(bytes) {
            self.posicao += bytes.len();
            Ok(valor)
        } else {
            Err(self.erro(&format!("esperava '{literal}'")))
        }
    }

    fn booleano(&mut self) -> Result<Valor, ErroJson> {
        if self.byte_atual() == Some(b't') {
            self.consumir_literal("true", Valor::Booleano(true))
        } else {
            self.consumir_literal("false", Valor::Booleano(false))
        }
    }

    fn nulo(&mut self) -> Result<Valor, ErroJson> {
        self.consumir_literal("null", Valor::Nulo)
    }

    fn numero(&mut self) -> Result<Valor, ErroJson> {
        let inicio = self.posicao;
        if self.byte_atual() == Some(b'-') {
            self.posicao += 1;
        }
        while let Some(b) = self.byte_atual() {
            // Aceita dígitos, ponto, expoente e sinal — depois delegamos o parse final ao f64.
            if b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || b == b'+' || b == b'-' {
                self.posicao += 1;
            } else {
                break;
            }
        }
        let trecho = std::str::from_utf8(&self.bytes[inicio..self.posicao])
            .map_err(|_| self.erro("número com bytes inválidos"))?;
        trecho
            .parse::<f64>()
            .map(Valor::Numero)
            .map_err(|_| self.erro("número malformado"))
    }

    fn texto(&mut self) -> Result<String, ErroJson> {
        // Consome a aspa de abertura.
        self.posicao += 1;
        let mut resultado = String::new();
        loop {
            match self.byte_atual() {
                None => return Err(self.erro("texto sem aspa de fechamento")),
                Some(b'"') => {
                    self.posicao += 1;
                    return Ok(resultado);
                }
                Some(b'\\') => {
                    self.posicao += 1;
                    let escape = self
                        .byte_atual()
                        .ok_or_else(|| self.erro("escape truncado"))?;
                    match escape {
                        b'"' => resultado.push('"'),
                        b'\\' => resultado.push('\\'),
                        b'/' => resultado.push('/'),
                        b'n' => resultado.push('\n'),
                        b't' => resultado.push('\t'),
                        b'r' => resultado.push('\r'),
                        b'b' => resultado.push('\u{0008}'),
                        b'f' => resultado.push('\u{000C}'),
                        b'u' => {
                            let caractere = self.escape_unicode()?;
                            resultado.push(caractere);
                            continue; // escape_unicode já avançou a posição
                        }
                        _ => return Err(self.erro("escape desconhecido")),
                    }
                    self.posicao += 1;
                }
                Some(_) => {
                    // Copia um caractere UTF-8 inteiro (pode ter múltiplos bytes).
                    let resto = std::str::from_utf8(&self.bytes[self.posicao..])
                        .map_err(|_| self.erro("texto com UTF-8 inválido"))?;
                    let caractere = resto
                        .chars()
                        .next()
                        .ok_or_else(|| self.erro("texto vazio inesperado"))?;
                    resultado.push(caractere);
                    self.posicao += caractere.len_utf8();
                }
            }
        }
    }

    /// Lê os 4 dígitos hexadecimais de um escape `\uXXXX` (incluindo pares substitutos).
    fn escape_unicode(&mut self) -> Result<char, ErroJson> {
        let alto = self.ler_hex4()?;
        // Par substituto UTF-16: faixa 0xD800..=0xDBFF precisa de um segundo \uXXXX.
        if (0xD800..=0xDBFF).contains(&alto) {
            if self.bytes[self.posicao..].starts_with(b"\\u") {
                self.posicao += 2;
                let baixo = self.ler_hex4()?;
                let codigo = 0x10000 + (((alto - 0xD800) as u32) << 10) + (baixo - 0xDC00) as u32;
                return char::from_u32(codigo).ok_or_else(|| self.erro("par substituto inválido"));
            }
            return Err(self.erro("substituto alto sem o baixo"));
        }
        char::from_u32(alto as u32).ok_or_else(|| self.erro("escape unicode inválido"))
    }

    fn ler_hex4(&mut self) -> Result<u16, ErroJson> {
        // A posição está logo após o 'u'. Lê 4 dígitos hex.
        self.posicao += 1; // pula o 'u'
        if self.posicao + 4 > self.bytes.len() {
            return Err(self.erro("escape unicode truncado"));
        }
        let trecho = std::str::from_utf8(&self.bytes[self.posicao..self.posicao + 4])
            .map_err(|_| self.erro("escape unicode com bytes inválidos"))?;
        let valor =
            u16::from_str_radix(trecho, 16).map_err(|_| self.erro("hex inválido no escape"))?;
        self.posicao += 4;
        Ok(valor)
    }

    fn lista(&mut self) -> Result<Valor, ErroJson> {
        self.posicao += 1; // consome '['
        let mut itens = Vec::new();
        self.pular_espacos();
        if self.byte_atual() == Some(b']') {
            self.posicao += 1;
            return Ok(Valor::Lista(itens));
        }
        loop {
            itens.push(self.valor()?);
            self.pular_espacos();
            match self.byte_atual() {
                Some(b',') => {
                    self.posicao += 1;
                }
                Some(b']') => {
                    self.posicao += 1;
                    return Ok(Valor::Lista(itens));
                }
                _ => return Err(self.erro("esperava ',' ou ']' na lista")),
            }
        }
    }

    fn objeto(&mut self) -> Result<Valor, ErroJson> {
        self.posicao += 1; // consome '{'
        let mut pares = Vec::new();
        self.pular_espacos();
        if self.byte_atual() == Some(b'}') {
            self.posicao += 1;
            return Ok(Valor::Objeto(pares));
        }
        loop {
            self.pular_espacos();
            if self.byte_atual() != Some(b'"') {
                return Err(self.erro("chave de objeto precisa ser texto"));
            }
            let chave = self.texto()?;
            self.pular_espacos();
            if self.byte_atual() != Some(b':') {
                return Err(self.erro("esperava ':' após a chave"));
            }
            self.posicao += 1;
            let valor = self.valor()?;
            pares.push((chave, valor));
            self.pular_espacos();
            match self.byte_atual() {
                Some(b',') => {
                    self.posicao += 1;
                }
                Some(b'}') => {
                    self.posicao += 1;
                    return Ok(Valor::Objeto(pares));
                }
                _ => return Err(self.erro("esperava ',' ou '}' no objeto")),
            }
        }
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn parseia_objeto_simples() {
        let valor = parsear(r#"{"nome": "rodolfo", "ativo": true, "n": 3}"#).unwrap();
        assert_eq!(
            valor.obter("nome").and_then(Valor::como_texto),
            Some("rodolfo")
        );
        assert_eq!(
            valor.obter("ativo").and_then(Valor::como_booleano),
            Some(true)
        );
        assert_eq!(valor.obter("n").and_then(Valor::como_numero), Some(3.0));
    }

    #[test]
    fn navega_lista_aninhada() {
        // Formato parecido com a resposta da API OpenAI-compat.
        let bruto = r#"{"choices":[{"message":{"content":"oi"}}]}"#;
        let valor = parsear(bruto).unwrap();
        let conteudo = valor
            .obter("choices")
            .and_then(|c| c.indice(0))
            .and_then(|c| c.obter("message"))
            .and_then(|m| m.obter("content"))
            .and_then(Valor::como_texto);
        assert_eq!(conteudo, Some("oi"));
    }

    #[test]
    fn trata_escapes() {
        let valor = parsear(r#""linha1\nlinha2\té""#).unwrap();
        assert_eq!(valor.como_texto(), Some("linha1\nlinha2\té"));
    }

    #[test]
    fn ida_e_volta_preserva_conteudo() {
        let original = r#"{"a":"b\"c","lista":[1,2,3],"nulo":null}"#;
        let valor = parsear(original).unwrap();
        let reserializado = valor.para_texto();
        // Reparsear o reserializado precisa dar o mesmo valor lógico.
        assert_eq!(parsear(&reserializado).unwrap(), valor);
    }

    #[test]
    fn rejeita_json_quebrado() {
        assert!(parsear("{").is_err());
        assert!(parsear(r#"{"a":}"#).is_err());
        assert!(parsear("[1,2,").is_err());
        assert!(parsear("").is_err());
    }
}
