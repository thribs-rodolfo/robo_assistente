//! Leitura da configuração dos provedores.
//!
//! A config mora FORA do repositório, em `/root/.secrets/roteador-provedores.json`
//! (contém chaves de API). Aqui só lemos e validamos. O repositório versiona apenas
//! um exemplo SEM chaves.
//!
//! Formato esperado (igual ao que o roteador Python já usava, para reaproveitar o arquivo):
//! ```json
//! {
//!   "ordem_fallback": ["claude", "ollama_local"],
//!   "provedores": {
//!     "ollama_local": {"tipo": "ollama", "url_base": "http://127.0.0.1:11434",
//!                      "modelo": "qwen2.5:1.5b", "timeout_segundos": 180, "habilitado": true}
//!   }
//! }
//! ```

use std::time::Duration;

use crate::erro::ErroRoteador;
use crate::json::{self, Valor};

/// Configuração de um provedor. Campos opcionais porque cada tipo usa um subconjunto:
/// o Ollama precisa de `url_base`/`modelo`; o Claude CLI precisa de `comando`; etc.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigProvedor {
    /// Nome lógico (a chave usada na `ordem_fallback`). Ex.: "ollama_local".
    pub nome: String,
    /// Tipo, que decide qual implementação instanciar. Ex.: "ollama", "claude_cli".
    pub tipo: String,
    /// URL base do serviço HTTP (Ollama, Groq, etc.). `None` para o Claude CLI.
    pub url_base: Option<String>,
    /// Nome do modelo a pedir ao provedor.
    pub modelo: Option<String>,
    /// Comando do CLI a executar (Claude). Padrão "claude" se ausente.
    pub comando: Option<String>,
    /// Chave de API (Groq/Gemini). `None` quando não se aplica.
    pub chave: Option<String>,
    /// Timeout em segundos para a chamada deste provedor.
    pub timeout: Duration,
    /// Se `false`, o provedor é pulado na pré-checagem (não tenta nem a rede).
    pub habilitado: bool,
}

/// Configuração completa do roteador: a ordem de fallback + os provedores declarados.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Ordem em que os provedores são tentados. O último deve ser o piso (Ollama local).
    pub ordem_fallback: Vec<String>,
    /// Provedores declarados, na ordem em que aparecem no arquivo.
    pub provedores: Vec<ConfigProvedor>,
}

impl Config {
    /// Acha a config de um provedor pelo nome lógico.
    pub fn provedor(&self, nome: &str) -> Option<&ConfigProvedor> {
        self.provedores.iter().find(|p| p.nome == nome)
    }
}

/// Caminho padrão da config (fora do repositório, com as chaves reais).
pub const CAMINHO_PADRAO: &str = "/root/.secrets/roteador-provedores.json";

/// Lê e parseia a config a partir de um caminho de arquivo.
pub fn carregar_de_arquivo(caminho: &str) -> Result<Config, ErroRoteador> {
    let conteudo = std::fs::read_to_string(caminho)
        .map_err(|e| ErroRoteador::Config(format!("não li '{caminho}': {e}")))?;
    interpretar(&conteudo)
}

/// Parseia a config a partir do texto JSON (separado da leitura de arquivo para ser testável).
pub fn interpretar(texto_json: &str) -> Result<Config, ErroRoteador> {
    let raiz = json::parsear(texto_json).map_err(|e| ErroRoteador::Config(e.to_string()))?;

    let ordem_fallback = raiz
        .obter("ordem_fallback")
        .and_then(Valor::como_lista)
        .ok_or_else(|| ErroRoteador::Config("falta a lista 'ordem_fallback'".into()))?
        .iter()
        .filter_map(Valor::como_texto)
        .map(str::to_string)
        .collect::<Vec<_>>();

    let provedores_objeto = match raiz.obter("provedores") {
        Some(Valor::Objeto(pares)) => pares,
        _ => return Err(ErroRoteador::Config("falta o objeto 'provedores'".into())),
    };

    let mut provedores = Vec::new();
    for (nome, config_valor) in provedores_objeto {
        provedores.push(interpretar_provedor(nome, config_valor)?);
    }

    Ok(Config {
        ordem_fallback,
        provedores,
    })
}

/// Extrai um `ConfigProvedor` do objeto JSON de um provedor.
fn interpretar_provedor(nome: &str, valor: &Valor) -> Result<ConfigProvedor, ErroRoteador> {
    let tipo = valor
        .obter("tipo")
        .and_then(Valor::como_texto)
        .ok_or_else(|| ErroRoteador::Config(format!("provedor '{nome}' sem 'tipo'")))?
        .to_string();

    // Timeout: aceita ausência (padrão 60s). Convertemos número -> segundos inteiros.
    let timeout_segundos = valor
        .obter("timeout_segundos")
        .and_then(Valor::como_numero)
        .map(|n| n.max(1.0) as u64)
        .unwrap_or(60);

    // 'habilitado' padrão true (igual ao Python), salvo quando explicitamente false.
    let habilitado = valor
        .obter("habilitado")
        .and_then(Valor::como_booleano)
        .unwrap_or(true);

    let texto_opcional = |chave: &str| {
        valor
            .obter(chave)
            .and_then(Valor::como_texto)
            .map(str::to_string)
    };

    Ok(ConfigProvedor {
        nome: nome.to_string(),
        tipo,
        url_base: texto_opcional("url_base"),
        modelo: texto_opcional("modelo"),
        comando: texto_opcional("comando"),
        chave: texto_opcional("chave"),
        timeout: Duration::from_secs(timeout_segundos),
        habilitado,
    })
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn interpreta_config_completa() {
        let bruto = r#"{
            "ordem_fallback": ["claude", "ollama_local"],
            "provedores": {
                "claude": {"tipo": "claude_cli", "comando": "claude", "timeout_segundos": 120, "habilitado": true},
                "ollama_local": {"tipo": "ollama", "url_base": "http://127.0.0.1:11434",
                                 "modelo": "qwen2.5:1.5b", "timeout_segundos": 180}
            }
        }"#;
        let config = interpretar(bruto).unwrap();
        assert_eq!(config.ordem_fallback, vec!["claude", "ollama_local"]);
        let ollama = config.provedor("ollama_local").unwrap();
        assert_eq!(ollama.tipo, "ollama");
        assert_eq!(ollama.modelo.as_deref(), Some("qwen2.5:1.5b"));
        assert_eq!(ollama.timeout, Duration::from_secs(180));
        assert!(ollama.habilitado); // padrão true quando ausente
    }

    #[test]
    fn rejeita_config_sem_provedores() {
        assert!(interpretar(r#"{"ordem_fallback": []}"#).is_err());
    }

    #[test]
    fn habilitado_false_e_respeitado() {
        let bruto = r#"{"ordem_fallback":["g"],"provedores":{"g":{"tipo":"openai_compat","habilitado":false}}}"#;
        let config = interpretar(bruto).unwrap();
        assert!(!config.provedor("g").unwrap().habilitado);
    }
}
