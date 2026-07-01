//! Diagnóstico do PISO — a rede de segurança do roteador.
//!
//! Toda a garantia do projeto ("o robô nunca fica mudo") repousa num único ponto: o
//! ÚLTIMO provedor da `ordem_fallback`, o piso (Ollama local). Se a cadeia de cima toda
//! falhar, é ele quem responde. Mas... e se o próprio piso estiver fora do ar? Aí o robô
//! fica mudo apesar de toda a lógica de fallback — e nenhum dos alarmes existentes pega
//! esse caso (eles medem "caí no piso DEMAIS", assumindo que o piso responde).
//!
//! Este módulo fecha esse buraco: uma checagem BARATA e SEGURA de que o piso está vivo e
//! com o modelo certo instalado. Barata porque usa `GET /api/tags` (só LISTA os modelos,
//! não roda inferência — não paga os ~33s de uma geração). Segura porque só sonda quando
//! o piso é do tipo `ollama` (custo zero, sem token): se por acaso o piso for outro tipo
//! (Claude/pagos), recusamos sondar de propósito — jamais disparamos o Claude "pra testar"
//! (licao-refresh-token-rotativo).
//!
//! Desenho testável (manifesto): a decisão é feita por funções PURAS
//! ([`avaliar_resposta_tags`], [`modelo_presente`], [`nomes_dos_modelos`]) que não tocam
//! rede; só [`verificar_piso`] abre o socket, e ela delega a decisão às puras.

use crate::config::{Config, ConfigProvedor};
use crate::http;
use crate::json::{self, Valor};

/// Resultado da checagem do piso. Cada variante conta uma história diferente para o
/// operador (e para o código de saída do binário).
#[derive(Debug, Clone, PartialEq)]
pub enum ResultadoPiso {
    /// Piso vivo e com o modelo esperado instalado — a rede de segurança está de pé.
    Saudavel { nome: String, modelo: String },
    /// Piso do tipo `resposta_fixa`: não tem o que sondar (é local, sem rede/processo) e,
    /// tendo `mensagem_fixa` configurada, está trivialmente vivo — nunca fica mudo.
    RespostaFixaViva { nome: String },
    /// Servidor respondeu, mas o modelo configurado NÃO está entre os instalados. A geração
    /// falharia; guardamos os modelos disponíveis para o operador ver o que há.
    ModeloFaltando {
        nome: String,
        modelo: String,
        disponiveis: Vec<String>,
    },
    /// O servidor do piso não respondeu como esperado (fora do ar, status != 200, corpo
    /// ilegível). A rede de segurança está comprometida.
    ForaDoAr { nome: String, detalhe: String },
    /// O piso está `"habilitado": false` na config — ele nem seria tentado. Grave: sem piso
    /// habilitado, o robô fica mudo se a cadeia de cima cair.
    Desabilitado { nome: String },
    /// O piso NÃO é do tipo `ollama`, então não o sondamos (não disparamos Claude/pagos só
    /// pra testar). Não é falha do piso — mas também não conseguimos CONFIRMAR que está vivo.
    NaoSondavel { nome: String, tipo: String },
    /// Não há piso identificável (ordem de fallback vazia, ou o último nome não tem provedor).
    SemPiso,
}

/// Gravidade do diagnóstico, para o binário escolher o código de saída (útil em cron/CI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severidade {
    /// Piso confirmado vivo. Código de saída 0.
    Ok,
    /// Piso comprometido (fora do ar, sem modelo, desabilitado, inexistente). Código 1.
    Comprometido,
    /// Não deu para verificar (piso não-Ollama). Código 2 — nem confirmou, nem falhou.
    NaoVerificado,
}

impl ResultadoPiso {
    /// Classifica o resultado em uma gravidade (para o código de saída do processo).
    pub fn severidade(&self) -> Severidade {
        match self {
            ResultadoPiso::Saudavel { .. } => Severidade::Ok,
            ResultadoPiso::RespostaFixaViva { .. } => Severidade::Ok,
            ResultadoPiso::NaoSondavel { .. } => Severidade::NaoVerificado,
            ResultadoPiso::ModeloFaltando { .. }
            | ResultadoPiso::ForaDoAr { .. }
            | ResultadoPiso::Desabilitado { .. }
            | ResultadoPiso::SemPiso => Severidade::Comprometido,
        }
    }
}

impl std::fmt::Display for ResultadoPiso {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResultadoPiso::Saudavel { nome, modelo } => {
                write!(
                    f,
                    "✅ Piso '{nome}' vivo e com o modelo '{modelo}' instalado."
                )
            }
            ResultadoPiso::RespostaFixaViva { nome } => write!(
                f,
                "✅ Piso '{nome}' é 'resposta_fixa' (texto local): sempre vivo, nunca fica mudo."
            ),
            ResultadoPiso::ModeloFaltando {
                nome,
                modelo,
                disponiveis,
            } => write!(
                f,
                "❌ Piso '{nome}' respondeu, mas o modelo '{modelo}' NÃO está instalado. \
                 Instalados: {}.",
                if disponiveis.is_empty() {
                    "nenhum".to_string()
                } else {
                    disponiveis.join(", ")
                }
            ),
            ResultadoPiso::ForaDoAr { nome, detalhe } => {
                write!(f, "❌ Piso '{nome}' fora do ar: {detalhe}.")
            }
            ResultadoPiso::Desabilitado { nome } => write!(
                f,
                "❌ Piso '{nome}' está DESABILITADO na config — o robô fica mudo se a cadeia cair."
            ),
            ResultadoPiso::NaoSondavel { nome, tipo } => write!(
                f,
                "⚠️ Piso '{nome}' é do tipo '{tipo}' (não-Ollama); não sondei para não \
                 disparar provedor pago/Claude. Vivacidade NÃO confirmada."
            ),
            ResultadoPiso::SemPiso => {
                write!(
                    f,
                    "❌ Nenhum piso identificável (ordem de fallback vazia?)."
                )
            }
        }
    }
}

/// Identifica o piso: o ÚLTIMO nome da `ordem_fallback` que tem um provedor declarado.
///
/// Por convenção do projeto, o piso é o último da cadeia (o Ollama local). Devolve `None`
/// se a ordem está vazia ou se o último nome não bate com nenhum provedor.
pub fn identificar_piso(config: &Config) -> Option<&ConfigProvedor> {
    let ultimo = config.ordem_fallback.last()?;
    config.provedor(ultimo)
}

/// Extrai a lista de nomes de modelos de uma resposta `/api/tags` do Ollama.
///
/// Formato: `{"models":[{"name":"qwen2.5:1.5b", ...}, ...]}`. Função PURA (sobre texto),
/// para ser testável sem rede. Erro de parse vira `Err` (nunca engolido silenciosamente).
pub fn nomes_dos_modelos(corpo_json: &str) -> Result<Vec<String>, String> {
    let raiz = json::parsear(corpo_json).map_err(|e| e.to_string())?;
    let modelos = raiz
        .obter("models")
        .and_then(Valor::como_lista)
        .ok_or_else(|| "resposta sem a lista 'models'".to_string())?;
    Ok(modelos
        .iter()
        .filter_map(|m| m.obter("name").and_then(Valor::como_texto))
        .map(str::to_string)
        .collect())
}

/// Decide se o modelo esperado está entre os instalados. Função PURA.
///
/// Compara por igualdade exata; e, se o esperado vier SEM tag (sem `:`), também aceita
/// qualquer instalado que comece por `esperado:` — porque o Ollama sempre guarda a tag
/// (ex.: config pede "qwen2.5" e o instalado é "qwen2.5:1.5b").
pub fn modelo_presente(instalados: &[String], esperado: &str) -> bool {
    if instalados.iter().any(|m| m == esperado) {
        return true;
    }
    if !esperado.contains(':') {
        let prefixo = format!("{esperado}:");
        return instalados.iter().any(|m| m.starts_with(&prefixo));
    }
    false
}

/// Avalia a resposta crua do `/api/tags` (status + corpo) contra o modelo esperado. PURA.
///
/// `nome` é o nome lógico do piso (só para compor o resultado). Status != 200, corpo
/// ilegível ou modelo ausente viram as variantes de falha correspondentes.
pub fn avaliar_resposta_tags(nome: &str, modelo: &str, status: u16, corpo: &str) -> ResultadoPiso {
    if status != 200 {
        return ResultadoPiso::ForaDoAr {
            nome: nome.to_string(),
            detalhe: format!("HTTP {status}"),
        };
    }
    let instalados = match nomes_dos_modelos(corpo) {
        Ok(lista) => lista,
        Err(motivo) => {
            return ResultadoPiso::ForaDoAr {
                nome: nome.to_string(),
                detalhe: format!("resposta ilegível: {motivo}"),
            };
        }
    };
    if modelo_presente(&instalados, modelo) {
        ResultadoPiso::Saudavel {
            nome: nome.to_string(),
            modelo: modelo.to_string(),
        }
    } else {
        ResultadoPiso::ModeloFaltando {
            nome: nome.to_string(),
            modelo: modelo.to_string(),
            disponiveis: instalados,
        }
    }
}

/// Verifica a saúde do piso da `config`. Esta é a ÚNICA função do módulo que toca a rede.
///
/// Fluxo: identifica o piso → se não há, `SemPiso`; se desabilitado, `Desabilitado`; se
/// não é `ollama`, `NaoSondavel` (não sondamos pago/Claude); senão, `GET /api/tags` e
/// delega a decisão para [`avaliar_resposta_tags`] (pura). Nunca roda inferência, nunca
/// dispara o Claude.
pub fn verificar_piso(config: &Config) -> ResultadoPiso {
    let piso = match identificar_piso(config) {
        Some(p) => p,
        None => return ResultadoPiso::SemPiso,
    };

    if !piso.habilitado {
        return ResultadoPiso::Desabilitado {
            nome: piso.nome.clone(),
        };
    }

    // Piso 'resposta_fixa': não há rede/processo a sondar. Está vivo por construção DESDE que
    // tenha 'mensagem_fixa' com texto — senão ficaria indisponível (pulado) e o robô mudo.
    if piso.tipo == "resposta_fixa" {
        let tem_texto = piso
            .mensagem_fixa
            .as_deref()
            .map(str::trim)
            .map(|t| !t.is_empty())
            .unwrap_or(false);
        return if tem_texto {
            ResultadoPiso::RespostaFixaViva {
                nome: piso.nome.clone(),
            }
        } else {
            ResultadoPiso::ForaDoAr {
                nome: piso.nome.clone(),
                detalhe: "piso resposta_fixa sem 'mensagem_fixa' — ficaria indisponível".into(),
            }
        };
    }

    // Só sondamos o piso Ollama (HTTP local, custo zero, sem token). Qualquer outro tipo
    // fica sem sondagem de propósito: não disparamos provedor pago/Claude só pra testar.
    if piso.tipo != "ollama" {
        return ResultadoPiso::NaoSondavel {
            nome: piso.nome.clone(),
            tipo: piso.tipo.clone(),
        };
    }

    let url_base = match piso.url_base.as_deref() {
        Some(u) => u.trim_end_matches('/'),
        None => {
            return ResultadoPiso::ForaDoAr {
                nome: piso.nome.clone(),
                detalhe: "piso Ollama sem 'url_base' na config".into(),
            }
        }
    };
    let modelo = match piso.modelo.as_deref() {
        Some(m) => m,
        None => {
            return ResultadoPiso::ForaDoAr {
                nome: piso.nome.clone(),
                detalhe: "piso Ollama sem 'modelo' na config".into(),
            }
        }
    };

    let url = format!("{url_base}/api/tags");
    match http::get(&url, piso.timeout) {
        Ok(resposta) => avaliar_resposta_tags(&piso.nome, modelo, resposta.status, &resposta.corpo),
        Err(falha) => ResultadoPiso::ForaDoAr {
            nome: piso.nome.clone(),
            detalhe: falha.to_string(),
        },
    }
}

#[cfg(test)]
mod testes {
    use super::*;
    use crate::config::interpretar;

    const TAGS_EXEMPLO: &str = r#"{"models":[
        {"name":"qwen2.5:1.5b","model":"qwen2.5:1.5b"},
        {"name":"tinyllama:latest","model":"tinyllama:latest"}
    ]}"#;

    #[test]
    fn extrai_nomes_dos_modelos() {
        let nomes = nomes_dos_modelos(TAGS_EXEMPLO).unwrap();
        assert_eq!(nomes, vec!["qwen2.5:1.5b", "tinyllama:latest"]);
    }

    #[test]
    fn corpo_sem_models_vira_erro() {
        assert!(nomes_dos_modelos(r#"{"outra":[]}"#).is_err());
        assert!(nomes_dos_modelos("não é json").is_err());
    }

    #[test]
    fn modelo_presente_por_igualdade_exata() {
        let instalados = vec!["qwen2.5:1.5b".to_string(), "tinyllama:latest".to_string()];
        assert!(modelo_presente(&instalados, "qwen2.5:1.5b"));
        assert!(!modelo_presente(&instalados, "llama3:70b"));
    }

    #[test]
    fn modelo_sem_tag_casa_por_prefixo() {
        // Config pede "qwen2.5" (sem tag); o instalado carrega a tag → deve casar.
        let instalados = vec!["qwen2.5:1.5b".to_string()];
        assert!(modelo_presente(&instalados, "qwen2.5"));
        // Mas com tag EXPLÍCITA diferente, não casa por prefixo.
        assert!(!modelo_presente(&instalados, "qwen2.5:7b"));
    }

    #[test]
    fn avalia_status_nao_200_como_fora_do_ar() {
        let r = avaliar_resposta_tags("ollama_local", "qwen2.5:1.5b", 503, "");
        assert_eq!(r.severidade(), Severidade::Comprometido);
        assert!(matches!(r, ResultadoPiso::ForaDoAr { .. }));
    }

    #[test]
    fn avalia_corpo_ilegivel_como_fora_do_ar() {
        let r = avaliar_resposta_tags("ollama_local", "qwen2.5:1.5b", 200, "lixo");
        assert!(matches!(r, ResultadoPiso::ForaDoAr { .. }));
    }

    #[test]
    fn avalia_saudavel_quando_modelo_presente() {
        let r = avaliar_resposta_tags("ollama_local", "qwen2.5:1.5b", 200, TAGS_EXEMPLO);
        assert_eq!(r.severidade(), Severidade::Ok);
        assert_eq!(
            r,
            ResultadoPiso::Saudavel {
                nome: "ollama_local".into(),
                modelo: "qwen2.5:1.5b".into()
            }
        );
    }

    #[test]
    fn avalia_modelo_faltando_lista_os_disponiveis() {
        let r = avaliar_resposta_tags("ollama_local", "llama3:70b", 200, TAGS_EXEMPLO);
        match r {
            ResultadoPiso::ModeloFaltando { disponiveis, .. } => {
                assert_eq!(disponiveis, vec!["qwen2.5:1.5b", "tinyllama:latest"]);
            }
            outro => panic!("esperava ModeloFaltando, veio {outro:?}"),
        }
    }

    #[test]
    fn identifica_o_ultimo_da_ordem_como_piso() {
        let bruto = r#"{
            "ordem_fallback":["claude","ollama_local"],
            "provedores":{
                "claude":{"tipo":"claude_cli"},
                "ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b"}
            }
        }"#;
        let config = interpretar(bruto).unwrap();
        let piso = identificar_piso(&config).unwrap();
        assert_eq!(piso.nome, "ollama_local");
    }

    #[test]
    fn piso_desabilitado_e_reportado_sem_tocar_a_rede() {
        // Piso Ollama, mas habilitado:false → Desabilitado (não abre socket nenhum).
        let bruto = r#"{
            "ordem_fallback":["ollama_local"],
            "provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"m","habilitado":false}}
        }"#;
        let config = interpretar(bruto).unwrap();
        let r = verificar_piso(&config);
        assert_eq!(
            r,
            ResultadoPiso::Desabilitado {
                nome: "ollama_local".into()
            }
        );
        assert_eq!(r.severidade(), Severidade::Comprometido);
    }

    #[test]
    fn piso_nao_ollama_nao_e_sondado() {
        // Piso do tipo claude_cli → NaoSondavel (jamais disparamos o Claude pra testar).
        let bruto = r#"{
            "ordem_fallback":["claude"],
            "provedores":{"claude":{"tipo":"claude_cli"}}
        }"#;
        let config = interpretar(bruto).unwrap();
        let r = verificar_piso(&config);
        assert_eq!(r.severidade(), Severidade::NaoVerificado);
        assert!(matches!(r, ResultadoPiso::NaoSondavel { .. }));
    }

    #[test]
    fn piso_resposta_fixa_com_texto_e_vivo_sem_tocar_rede() {
        // Piso resposta_fixa com mensagem → vivo por construção (nenhum socket é aberto).
        let bruto = r#"{
            "ordem_fallback":["cortesia"],
            "provedores":{"cortesia":{"tipo":"resposta_fixa","mensagem_fixa":"volto já"}}
        }"#;
        let config = interpretar(bruto).unwrap();
        let r = verificar_piso(&config);
        assert_eq!(r.severidade(), Severidade::Ok);
        assert_eq!(
            r,
            ResultadoPiso::RespostaFixaViva {
                nome: "cortesia".into()
            }
        );
    }

    #[test]
    fn piso_resposta_fixa_sem_texto_e_comprometido() {
        // Sem 'mensagem_fixa' ele ficaria indisponível (pulado) → piso comprometido.
        let bruto = r#"{
            "ordem_fallback":["cortesia"],
            "provedores":{"cortesia":{"tipo":"resposta_fixa"}}
        }"#;
        let config = interpretar(bruto).unwrap();
        let r = verificar_piso(&config);
        assert_eq!(r.severidade(), Severidade::Comprometido);
        assert!(matches!(r, ResultadoPiso::ForaDoAr { .. }));
    }

    #[test]
    fn sem_piso_quando_ordem_vazia() {
        let bruto = r#"{"ordem_fallback":[],"provedores":{"x":{"tipo":"ollama"}}}"#;
        let config = interpretar(bruto).unwrap();
        assert_eq!(verificar_piso(&config), ResultadoPiso::SemPiso);
    }
}
