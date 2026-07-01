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

impl FalhaProvedor {
    /// Esta falha indica que o **provedor** está indisponível/rejeitando (fora do ar,
    /// token caído, rate limit, erro interno) — ou foi um problema **daquela mensagem
    /// específica** (o provedor está de pé e respondeu, só não deu certo para este pedido)?
    ///
    /// Serve ao disjuntor (ver [`crate::disjuntor`]): só faz sentido "abrir o circuito"
    /// (parar de tentar o provedor por um tempo) quando o PROVEDOR está indisponível. Se
    /// abríssemos o circuito por uma falha da mensagem (ex.: um HTTP 400 porque aquele texto
    /// veio malformado), puniríamos um provedor SÃO — ele seria pulado nas próximas mensagens
    /// boas, jogando o robô no piso (Ollama) à toa. Isso é exatamente o oposto do objetivo do
    /// projeto: depender MENOS do piso.
    ///
    /// Classificação (por que cada uma):
    /// - [`FalhaProvedor::Rede`] → o provedor não respondeu (host fora, timeout de socket):
    ///   indisponível. **Conta.**
    /// - [`FalhaProvedor::Processo`] → `claude --print` falhou (token caído, timeout do
    ///   processo): o provedor está fora pra valer. **Conta.**
    /// - [`FalhaProvedor::Http`] → depende do status:
    ///   - `400` (pedido malformado), `404` (rota/modelo não encontrado), `413` (corpo grande
    ///     demais), `422` (conteúdo não-processável) → problema DESTA requisição. **Não conta.**
    ///   - Demais (`401`/`403` auth, `408` timeout, `429` rate limit, `5xx` erro do servidor)
    ///     → o provedor está fora/rejeitando de forma persistente. **Conta.**
    /// - [`FalhaProvedor::RespostaVazia`] → o provedor respondeu 200, só veio vazio (provável
    ///   coisa deste prompt). Está de pé. **Não conta.**
    /// - [`FalhaProvedor::RespostaInvalida`] → respondeu, mas em formato inesperado; o provedor
    ///   está no ar (falha de contrato/parse, não de disponibilidade). **Não conta.**
    /// - [`FalhaProvedor::Indisponivel`] → veio de uma pré-checagem/config (sem `url_base`,
    ///   sem chave): é problema de configuração, não de saúde do provedor no ar. **Não conta.**
    ///
    /// Função **pura** (não olha relógio, disco nem rede) → fácil de testar.
    pub fn indica_provedor_indisponivel(&self) -> bool {
        match self {
            FalhaProvedor::Rede(_) => true,
            FalhaProvedor::Processo(_) => true,
            FalhaProvedor::Http { status, .. } => !falha_da_requisicao(*status),
            FalhaProvedor::RespostaVazia => false,
            FalhaProvedor::RespostaInvalida(_) => false,
            FalhaProvedor::Indisponivel(_) => false,
        }
    }
}

/// Um status HTTP que representa problema DA requisição atual (não do provedor em si).
/// Nesses casos a próxima mensagem pode passar numa boa, então não vale abrir o disjuntor.
fn falha_da_requisicao(status: u16) -> bool {
    matches!(status, 400 | 404 | 413 | 422)
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

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn falhas_de_disponibilidade_contam_pro_disjuntor() {
        // Provedor fora/rejeitando: vale abrir o circuito.
        assert!(FalhaProvedor::Rede("timeout".into()).indica_provedor_indisponivel());
        assert!(FalhaProvedor::Processo("código 1".into()).indica_provedor_indisponivel());
        // 401/403 auth (chave/token ruim), 408, 429 rate limit, 5xx: provedor indisponível.
        for status in [401, 403, 408, 429, 500, 502, 503] {
            assert!(
                FalhaProvedor::Http {
                    status,
                    corpo: String::new(),
                }
                .indica_provedor_indisponivel(),
                "HTTP {status} devia contar como indisponibilidade do provedor"
            );
        }
    }

    #[test]
    fn falhas_da_mensagem_nao_contam_pro_disjuntor() {
        // O provedor respondeu; o problema é DESTA requisição — não punir o provedor são.
        for status in [400, 404, 413, 422] {
            assert!(
                !FalhaProvedor::Http {
                    status,
                    corpo: String::new(),
                }
                .indica_provedor_indisponivel(),
                "HTTP {status} devia ser tratado como falha da mensagem"
            );
        }
        assert!(!FalhaProvedor::RespostaVazia.indica_provedor_indisponivel());
        assert!(!FalhaProvedor::RespostaInvalida("sem campo".into()).indica_provedor_indisponivel());
        // Config (sem chave/url_base) não é saúde do provedor no ar.
        assert!(!FalhaProvedor::Indisponivel("sem chave".into()).indica_provedor_indisponivel());
    }
}
