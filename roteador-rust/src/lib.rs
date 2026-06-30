//! Roteador de provedores — cérebro agnóstico da ponte/agente.
//!
//! A ponte deixa de chamar `claude --print` direto. Passa a chamar [`rotear`], que tenta
//! os provedores na ordem de fallback configurada. Se um falha (sem chave, 401, 429,
//! timeout, erro), cai para o próximo. O Ollama local fica SEMPRE por último: piso de
//! emergência, custo zero, nunca deixa o robô mudo.
//!
//! Filosofia (WORKSPACE_RULES "Como escrevemos código"): zero dependências (JSON, HTTP e
//! processo escritos à mão na stdlib), agnóstico (trait [`Provedor`]), testável, erros
//! tipados com `Result`, sem `unwrap`/`panic` em produção, telemetria clara.

pub mod alerta;
pub mod config;
pub mod duracao;
pub mod erro;
pub mod http;
pub mod https;
pub mod json;
pub mod metricas;
pub mod ponte;
pub mod prompt;
pub mod provedor;
pub mod servidor_http;
pub mod telemetria;

pub use config::{carregar_de_arquivo, Config, CAMINHO_PADRAO};
pub use erro::{ErroRoteador, FalhaProvedor};
pub use prompt::{Autor, Contexto, Turno};
pub use provedor::Provedor;

/// Resultado de um roteamento bem-sucedido: o texto da resposta e quem respondeu.
#[derive(Debug, Clone, PartialEq)]
pub struct RespostaRoteada {
    /// O texto que o provedor devolveu.
    pub texto: String,
    /// Nome lógico do provedor que respondeu (telemetria: medir dependência real).
    pub provedor: String,
}

/// Roteia uma mensagem pela cadeia de fallback definida na `config`.
///
/// Tenta cada provedor da `ordem_fallback`, em ordem. Para cada um:
/// 1. pré-checa `disponivel()` (barato: habilitado? tem chave?);
/// 2. se passar, chama `responder()`;
/// 3. em qualquer falha recuperável, registra o motivo e cai para o próximo.
///
/// Devolve [`RespostaRoteada`] no primeiro sucesso. Só devolve `Err` se TODOS falharem
/// (não deve acontecer enquanto o Ollama local estiver vivo no fim da cadeia).
pub fn rotear(
    mensagem: &str,
    contexto: &Contexto,
    config: &Config,
) -> Result<RespostaRoteada, ErroRoteador> {
    if config.ordem_fallback.is_empty() {
        return Err(ErroRoteador::SemProvedores);
    }

    // Acumula o motivo de cada falha para telemetria e para a mensagem final de erro.
    let mut motivos: Vec<String> = Vec::new();
    let mut algum_provedor_construido = false;

    for nome in &config.ordem_fallback {
        // Acha a config deste provedor; se faltar, anota e segue (não derruba o roteador).
        let config_provedor = match config.provedor(nome) {
            Some(c) => c,
            None => {
                let motivo = format!("{nome}: na ordem mas sem configuração");
                telemetria::registrar(&motivo);
                motivos.push(motivo);
                continue;
            }
        };

        // Constrói a implementação concreta a partir do `tipo`.
        let provedor = match provedor::construir(config_provedor) {
            Some(p) => p,
            None => {
                let motivo = format!("{nome}: tipo '{}' desconhecido", config_provedor.tipo);
                telemetria::registrar(&motivo);
                motivos.push(motivo);
                continue;
            }
        };
        algum_provedor_construido = true;

        // Pré-checagem: desabilitado ou sem chave? Pula sem gastar rede.
        if let Err(falha) = provedor.disponivel() {
            let motivo = format!("{nome}: {falha}");
            telemetria::registrar(&format!("[pula] {motivo}"));
            motivos.push(motivo);
            continue;
        }

        // Tentativa real. Medimos a latência para a telemetria (custo/performance):
        // saber QUANTO cada provedor demora é tão útil quanto saber QUEM respondeu.
        let inicio = std::time::Instant::now();
        let resultado = provedor.responder(mensagem, contexto);
        let ms = inicio.elapsed().as_millis();
        match resultado {
            Ok(texto) => {
                telemetria::registrar(&format!("[ok] respondido por '{nome}' em {ms}ms"));
                return Ok(RespostaRoteada {
                    texto,
                    provedor: nome.clone(),
                });
            }
            Err(falha) => {
                let motivo = format!("{nome}: {falha}");
                telemetria::registrar(&format!(
                    "[falha] {motivo} (após {ms}ms) — caindo pro próximo"
                ));
                motivos.push(motivo);
            }
        }
    }

    if !algum_provedor_construido {
        return Err(ErroRoteador::SemProvedores);
    }
    Err(ErroRoteador::TodosFalharam(motivos))
}

/// Atalho que carrega a config do caminho padrão e roteia. Útil para o binário/ponte.
pub fn rotear_com_config_padrao(
    mensagem: &str,
    contexto: &Contexto,
) -> Result<RespostaRoteada, ErroRoteador> {
    let config = carregar_de_arquivo(CAMINHO_PADRAO)?;
    rotear(mensagem, contexto, &config)
}

#[cfg(test)]
mod testes {
    use super::*;
    use crate::config::interpretar;

    #[test]
    fn ordem_vazia_da_erro() {
        let config = Config {
            ordem_fallback: vec![],
            provedores: vec![],
        };
        let erro = rotear("oi", &Contexto::vazio(), &config).unwrap_err();
        assert_eq!(erro, ErroRoteador::SemProvedores);
    }

    #[test]
    fn provedor_na_ordem_sem_config_e_pulado_e_falha_no_fim() {
        // 'fantasma' está na ordem mas não nos provedores; nenhum provedor é construído.
        let config = interpretar(r#"{"ordem_fallback":["fantasma"],"provedores":{}}"#).unwrap();
        let erro = rotear("oi", &Contexto::vazio(), &config).unwrap_err();
        assert_eq!(erro, ErroRoteador::SemProvedores);
    }

    #[test]
    fn cai_para_ollama_quando_groq_indisponivel() {
        // Groq habilitado mas sem chave de HTTPS -> indisponível; a cadeia tenta o Ollama
        // em seguida. O Ollama aponta para uma porta morta de propósito, então também
        // falha — mas o teste prova que a CADEIA andou pelos dois (ambos os motivos saem).
        let config = interpretar(
            r#"{
                "ordem_fallback": ["groq", "ollama_local"],
                "provedores": {
                    "groq": {"tipo":"openai_compat","chave":"x","habilitado":true},
                    "ollama_local": {"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}
                }
            }"#,
        )
        .unwrap();
        match rotear("oi", &Contexto::vazio(), &config).unwrap_err() {
            ErroRoteador::TodosFalharam(motivos) => {
                assert_eq!(motivos.len(), 2);
                assert!(motivos[0].starts_with("groq:"));
                assert!(motivos[1].starts_with("ollama_local:"));
            }
            outro => panic!("esperava TodosFalharam, veio {outro:?}"),
        }
    }
}
