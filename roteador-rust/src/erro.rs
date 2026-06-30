//! Tipos de erro do roteador.
//!
//! Seguimos o manifesto: erros são VALORES tipados (`Result`/`enum`), nunca exceções
//! nem `panic!`/`unwrap()` em caminho de produção. Cada provedor que falha devolve uma
//! `FalhaProvedor`, e o roteador usa isso para decidir cair para o próximo da cadeia.

use std::fmt;

/// Falha recuperável de um provedor. Quando um provedor devolve isto, o roteador
/// registra o motivo (telemetria) e tenta o próximo provedor da ordem de fallback.
#[derive(Debug, Clone, PartialEq)]
pub enum FalhaProvedor {
    /// Pré-checagem reprovou: provedor desabilitado na config ou sem chave.
    Indisponivel(String),
    /// Erro de rede/conexão (Ollama fora do ar, host inacessível, timeout de socket).
    Rede(String),
    /// O serviço respondeu com status HTTP de erro (ex.: 401 sem cota, 429 rate limit).
    Http { status: u16, corpo: String },
    /// O processo externo (`claude --print`) falhou: não encontrado, código != 0, timeout.
    Processo(String),
    /// A resposta veio, mas em formato inesperado (JSON sem o campo que esperávamos).
    RespostaInvalida(String),
    /// O provedor respondeu, porém com texto vazio — inútil, então tratamos como falha.
    RespostaVazia,
}

impl fmt::Display for FalhaProvedor {
    fn fmt(&self, formatador: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FalhaProvedor::Indisponivel(motivo) => write!(formatador, "indisponível: {motivo}"),
            FalhaProvedor::Rede(motivo) => write!(formatador, "rede: {motivo}"),
            FalhaProvedor::Http { status, corpo } => {
                write!(formatador, "http {status}: {}", recortar(corpo, 200))
            }
            FalhaProvedor::Processo(motivo) => write!(formatador, "processo: {motivo}"),
            FalhaProvedor::RespostaInvalida(motivo) => {
                write!(formatador, "resposta inválida: {motivo}")
            }
            FalhaProvedor::RespostaVazia => write!(formatador, "resposta vazia"),
        }
    }
}

impl std::error::Error for FalhaProvedor {}

/// Recorta um texto para no máximo `limite` caracteres (evita log gigante de corpo de erro).
fn recortar(texto: &str, limite: usize) -> String {
    if texto.chars().count() <= limite {
        texto.to_string()
    } else {
        let recortado: String = texto.chars().take(limite).collect();
        format!("{recortado}…")
    }
}

/// Erro de nível do roteador (não de um provedor específico). Acontece quando a config
/// está ruim ou — caso extremo — TODOS os provedores falharam.
#[derive(Debug, Clone, PartialEq)]
pub enum ErroRoteador {
    /// Não foi possível ler/parsear a config dos provedores.
    Config(String),
    /// A `ordem_fallback` ficou vazia ou nenhum provedor pôde ser construído.
    SemProvedores,
    /// Todos os provedores da cadeia falharam. Carrega o motivo de cada um (telemetria).
    TodosFalharam(Vec<String>),
}

impl fmt::Display for ErroRoteador {
    fn fmt(&self, formatador: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErroRoteador::Config(motivo) => write!(formatador, "config inválida: {motivo}"),
            ErroRoteador::SemProvedores => {
                write!(formatador, "nenhum provedor configurado na ordem_fallback")
            }
            ErroRoteador::TodosFalharam(motivos) => {
                write!(
                    formatador,
                    "todos os provedores falharam: {}",
                    motivos.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for ErroRoteador {}
