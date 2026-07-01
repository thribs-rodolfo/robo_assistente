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

/// Ajustes do disjuntor (circuit breaker) — ver [`crate::disjuntor`].
///
/// Bloco OPCIONAL no JSON (chave `"disjuntor"`). Ausente => `Default` = DESLIGADO, e o
/// roteamento se comporta EXATAMENTE como antes (risco zero para quem não configura).
/// Para ligar: `"disjuntor": {"habilitado": true}` (limiar/cooldown têm padrões sensatos).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDisjuntor {
    /// Liga/desliga o disjuntor. Desligado, o roteador nem lê/grava o arquivo de estado.
    pub habilitado: bool,
    /// Quantas falhas SEGUIDAS abrem o circuito de um provedor.
    pub limiar_falhas: u32,
    /// Por quantos segundos o circuito fica aberto (provedor pulado) antes do meio-aberto.
    pub cooldown_segundos: u64,
    /// Onde persistir o estado entre mensagens (a ponte é um processo vivo, mas o binário
    /// pode reiniciar; o arquivo dá continuidade). Fora do repositório.
    pub caminho_estado: String,
}

/// Caminho padrão do estado do disjuntor (fora do repositório; efêmero/operacional).
pub const CAMINHO_ESTADO_DISJUNTOR_PADRAO: &str = "/var/log/roteador-disjuntor.estado";

impl Default for ConfigDisjuntor {
    fn default() -> Self {
        // Padrões conservadores: desligado, e — quando ligado — 3 falhas abrem por 60s.
        ConfigDisjuntor {
            habilitado: false,
            limiar_falhas: 3,
            cooldown_segundos: 60,
            caminho_estado: CAMINHO_ESTADO_DISJUNTOR_PADRAO.to_string(),
        }
    }
}

/// Configuração completa do roteador: a ordem de fallback + os provedores declarados.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Ordem em que os provedores são tentados. O último deve ser o piso (Ollama local).
    pub ordem_fallback: Vec<String>,
    /// Provedores declarados, na ordem em que aparecem no arquivo.
    pub provedores: Vec<ConfigProvedor>,
    /// Ajustes do disjuntor. Ausente no JSON => `Default` (desligado).
    pub disjuntor: ConfigDisjuntor,
    /// Onde gravar a telemetria (qual provedor respondeu, por que caiu). Ausente no JSON =>
    /// o log de produção ([`crate::telemetria::ARQUIVO_LOG`]). Existe para NÃO ter um caminho
    /// global escondido: os testes apontam para um arquivo temporário e o roteamento fica
    /// hermético — antes, rodar `cargo test` sujava o log de produção e contaminava as
    /// métricas reais do `bin/metricas` (a medida de "% no piso" que é o objetivo do projeto).
    pub telemetria_log: String,
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

    let disjuntor = interpretar_disjuntor(raiz.obter("disjuntor"));

    // Caminho da telemetria: opcional. Ausente => o log de produção. Só quem escreve teste
    // aponta para outro lugar (arquivo temporário) para não sujar as métricas reais.
    let telemetria_log = raiz
        .obter("telemetria_log")
        .and_then(Valor::como_texto)
        .map(str::to_string)
        .unwrap_or_else(|| crate::telemetria::ARQUIVO_LOG.to_string());

    Ok(Config {
        ordem_fallback,
        provedores,
        disjuntor,
        telemetria_log,
    })
}

/// Extrai o bloco `disjuntor` (opcional). Ausente ou não-objeto => `Default` (desligado).
/// Cada campo cai no padrão quando falta, então `{"habilitado": true}` já basta para ligar.
fn interpretar_disjuntor(valor: Option<&Valor>) -> ConfigDisjuntor {
    let padrao = ConfigDisjuntor::default();
    let valor = match valor {
        Some(v) => v,
        None => return padrao,
    };

    let habilitado = valor
        .obter("habilitado")
        .and_then(Valor::como_booleano)
        .unwrap_or(padrao.habilitado);
    // Limiar mínimo 1: "0 falhas abrem" não faz sentido (abriria sem nunca tentar).
    let limiar_falhas = valor
        .obter("limiar_falhas")
        .and_then(Valor::como_numero)
        .map(|n| (n.max(1.0)) as u32)
        .unwrap_or(padrao.limiar_falhas);
    let cooldown_segundos = valor
        .obter("cooldown_segundos")
        .and_then(Valor::como_numero)
        .map(|n| (n.max(1.0)) as u64)
        .unwrap_or(padrao.cooldown_segundos);
    let caminho_estado = valor
        .obter("caminho_estado")
        .and_then(Valor::como_texto)
        .map(str::to_string)
        .unwrap_or(padrao.caminho_estado);

    ConfigDisjuntor {
        habilitado,
        limiar_falhas,
        cooldown_segundos,
        caminho_estado,
    }
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

    #[test]
    fn disjuntor_ausente_vem_desligado_por_padrao() {
        let bruto = r#"{"ordem_fallback":["g"],"provedores":{"g":{"tipo":"ollama"}}}"#;
        let config = interpretar(bruto).unwrap();
        assert!(!config.disjuntor.habilitado);
        assert_eq!(config.disjuntor, ConfigDisjuntor::default());
    }

    #[test]
    fn disjuntor_liga_e_le_campos() {
        let bruto = r#"{
            "ordem_fallback":["g"],
            "provedores":{"g":{"tipo":"ollama"}},
            "disjuntor":{"habilitado":true,"limiar_falhas":5,"cooldown_segundos":120,
                         "caminho_estado":"/tmp/x.estado"}
        }"#;
        let config = interpretar(bruto).unwrap();
        assert!(config.disjuntor.habilitado);
        assert_eq!(config.disjuntor.limiar_falhas, 5);
        assert_eq!(config.disjuntor.cooldown_segundos, 120);
        assert_eq!(config.disjuntor.caminho_estado, "/tmp/x.estado");
    }

    #[test]
    fn disjuntor_so_com_habilitado_usa_padroes() {
        // Ligar sem detalhar campos deve herdar limiar/cooldown padrão.
        let bruto = r#"{"ordem_fallback":["g"],"provedores":{"g":{"tipo":"ollama"}},"disjuntor":{"habilitado":true}}"#;
        let config = interpretar(bruto).unwrap();
        assert!(config.disjuntor.habilitado);
        assert_eq!(config.disjuntor.limiar_falhas, 3);
        assert_eq!(config.disjuntor.cooldown_segundos, 60);
    }

    #[test]
    fn telemetria_log_ausente_usa_o_padrao_de_producao() {
        let bruto = r#"{"ordem_fallback":["g"],"provedores":{"g":{"tipo":"ollama"}}}"#;
        let config = interpretar(bruto).unwrap();
        assert_eq!(config.telemetria_log, crate::telemetria::ARQUIVO_LOG);
    }

    #[test]
    fn telemetria_log_pode_ser_sobrescrito() {
        let bruto = r#"{"ordem_fallback":["g"],"provedores":{"g":{"tipo":"ollama"}},"telemetria_log":"/tmp/x.log"}"#;
        let config = interpretar(bruto).unwrap();
        assert_eq!(config.telemetria_log, "/tmp/x.log");
    }

    #[test]
    fn disjuntor_limiar_zero_vira_um() {
        let bruto = r#"{"ordem_fallback":["g"],"provedores":{"g":{"tipo":"ollama"}},"disjuntor":{"habilitado":true,"limiar_falhas":0}}"#;
        let config = interpretar(bruto).unwrap();
        assert_eq!(config.disjuntor.limiar_falhas, 1);
    }
}
