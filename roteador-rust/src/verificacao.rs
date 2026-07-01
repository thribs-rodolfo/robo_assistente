//! Verificação ESTÁTICA da configuração do roteador (um "doutor" de config).
//!
//! Toda a garantia do projeto — "o robô NUNCA fica mudo porque o piso (Ollama local)
//! responde quando todo o resto falha" — depende de uma config bem-formada. Se alguém
//! desabilita o piso, tira ele da ordem, põe um provedor que exige chave como último, ou
//! cita na `ordem_fallback` um nome que não existe, a garantia quebra em SILÊNCIO: só se
//! descobre em produção, quando a cadeia inteira cai e o [`crate::rotear`] devolve
//! `TodosFalharam` — com o bot vivo e o usuário mudo.
//!
//! Este módulo pega essa classe de erro ANTES de ir para o ar. São só funções PURAS sobre
//! a [`Config`] já parseada: nenhuma rede, nenhum processo, NENHUM provedor é construído
//! para valer nem disparado. Rodar isto é 100% seguro (jamais toca o Claude —
//! `licao-refresh-token-rotativo`).
//!
//! A checagem sabe distinguir dois graus:
//! - [`Severidade::Erro`]  — quebra o roteamento ou a garantia do piso (precisa corrigir).
//! - [`Severidade::Aviso`] — funciona, mas quase certamente é engano ou desperdício.

use std::collections::BTreeMap;
use std::fmt;

use crate::config::Config;
use crate::provedor;

/// Gravidade de um achado da verificação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severidade {
    /// Quebra o roteamento ou a garantia "nunca fica mudo". Precisa corrigir.
    Erro,
    /// Config funciona, mas o achado quase certamente é engano ou desperdício.
    Aviso,
}

/// Um problema encontrado na config, com sua gravidade e uma explicação em pt-BR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Achado {
    /// Quão grave é.
    pub severidade: Severidade,
    /// Mensagem legível explicando o problema (e, quando dá, como corrigir).
    pub mensagem: String,
}

impl Achado {
    /// Cria um achado de erro (quebra roteamento/garantia).
    fn erro(mensagem: impl Into<String>) -> Achado {
        Achado {
            severidade: Severidade::Erro,
            mensagem: mensagem.into(),
        }
    }

    /// Cria um achado de aviso (funciona, mas provável engano).
    fn aviso(mensagem: impl Into<String>) -> Achado {
        Achado {
            severidade: Severidade::Aviso,
            mensagem: mensagem.into(),
        }
    }
}

impl fmt::Display for Achado {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Ícone + rótulo alinhado para a saída do binário ficar legível de bater o olho.
        let etiqueta = match self.severidade {
            Severidade::Erro => "❌ ERRO ",
            Severidade::Aviso => "⚠️  AVISO",
        };
        write!(f, "{etiqueta}  {}", self.mensagem)
    }
}

/// Tipos cujo provedor DEPENDE de uma chave de API externa: sem chave, a pré-checagem
/// `disponivel()` já os pula em toda mensagem. Espelha a lógica de `disponivel()` de
/// `ProvedorOpenAiCompat`/`ProvedorGeminiRest` (ver [`crate::provedor`]). Um provedor
/// assim NÃO serve de piso: o piso não pode depender de chave/cota que pode faltar.
fn tipo_exige_chave(tipo: &str) -> bool {
    matches!(tipo, "openai_compat" | "gemini_rest")
}

/// Verifica a config e devolve todos os achados (vazio = tudo certo).
///
/// Função PURA: não lê disco, não abre rede, não constrói provedor para valer. A ordem
/// dos achados é determinística (útil para testes e para uma saída estável).
pub fn verificar(config: &Config) -> Vec<Achado> {
    let mut achados = Vec::new();

    // 1. Ordem vazia: sem ninguém para tentar, o robô fica mudo. Sem ordem, o resto das
    //    checagens não faz sentido, então paramos aqui.
    if config.ordem_fallback.is_empty() {
        achados.push(Achado::erro(
            "ordem_fallback vazia: nenhum provedor será tentado, o robô fica mudo",
        ));
        return achados;
    }

    // 2. Cada nome citado na ordem precisa existir em 'provedores'; e citar o mesmo nome
    //    duas vezes é redundante. Contamos uma vez por nome (BTreeMap = ordem estável e
    //    sem relatar o mesmo nome repetido várias vezes).
    let mut contagem_na_ordem: BTreeMap<&str, u32> = BTreeMap::new();
    for nome in &config.ordem_fallback {
        *contagem_na_ordem.entry(nome.as_str()).or_insert(0) += 1;
    }
    for (nome, quantas_vezes) in &contagem_na_ordem {
        if config.provedor(nome).is_none() {
            achados.push(Achado::erro(format!(
                "'{nome}' está na ordem_fallback mas não foi declarado em 'provedores'"
            )));
        }
        if *quantas_vezes > 1 {
            achados.push(Achado::aviso(format!(
                "'{nome}' aparece {quantas_vezes} vezes na ordem_fallback (redundante)"
            )));
        }
    }

    // 3. Provedor declarado mas fora da ordem: nunca será usado (config morta).
    for provedor_config in &config.provedores {
        if !contagem_na_ordem.contains_key(provedor_config.nome.as_str()) {
            achados.push(Achado::aviso(format!(
                "provedor '{}' foi declarado mas não está na ordem_fallback (nunca será usado)",
                provedor_config.nome
            )));
        }
    }

    // 4. Sanidade de cada provedor que ESTÁ na ordem (o resto já foi avisado no item 3).
    for provedor_config in &config.provedores {
        if !contagem_na_ordem.contains_key(provedor_config.nome.as_str()) {
            continue;
        }
        let nome = &provedor_config.nome;

        // Tipo desconhecido? Fonte da verdade é `provedor::construir` (o mesmo `match` que
        // o roteador usa): se ele não reconhece, o roteador vai pular esse provedor sempre.
        // `construir` só guarda a config — não abre rede nem dispara nada.
        if provedor::construir(provedor_config).is_none() {
            achados.push(Achado::erro(format!(
                "provedor '{nome}': tipo '{}' desconhecido (o roteador vai pular sempre)",
                provedor_config.tipo
            )));
            continue; // sem tipo válido, checar campos não faz sentido
        }

        // Campos obrigatórios por tipo: sem eles o `responder()` falha em TODA tentativa.
        let sem = |valor: &Option<String>| valor.as_deref().unwrap_or("").is_empty();
        match provedor_config.tipo.as_str() {
            "ollama" => {
                if sem(&provedor_config.url_base) {
                    achados.push(Achado::erro(format!(
                        "ollama '{nome}' sem 'url_base': falharia em toda tentativa"
                    )));
                }
                if sem(&provedor_config.modelo) {
                    achados.push(Achado::erro(format!(
                        "ollama '{nome}' sem 'modelo': falharia em toda tentativa"
                    )));
                }
            }
            "openai_compat" => {
                if sem(&provedor_config.url_base) {
                    achados.push(Achado::erro(format!(
                        "provedor '{nome}' (openai_compat) sem 'url_base': falharia em toda tentativa"
                    )));
                }
                if sem(&provedor_config.modelo) {
                    achados.push(Achado::erro(format!(
                        "provedor '{nome}' (openai_compat) sem 'modelo': falharia em toda tentativa"
                    )));
                }
            }
            // Gemini só exige 'modelo' — guard no próprio match (evita `if` aninhado).
            "gemini_rest" if sem(&provedor_config.modelo) => {
                achados.push(Achado::erro(format!(
                    "gemini '{nome}' sem 'modelo': falharia em toda tentativa"
                )));
            }
            _ => {} // gemini com modelo / claude_cli: nada obrigatório a mais
        }

        // Habilitado + exige chave + sem chave: o roteador o pula em toda mensagem. É o
        // estado ESPERADO do Groq/Gemini hoje (sem chave), por isso é aviso, não erro.
        if provedor_config.habilitado
            && tipo_exige_chave(&provedor_config.tipo)
            && sem(&provedor_config.chave)
        {
            achados.push(Achado::aviso(format!(
                "provedor '{nome}' está habilitado mas sem 'chave': será pulado em toda mensagem (indisponível)"
            )));
        }
    }

    // 5. O PISO (último da ordem) — o coração da garantia "nunca fica mudo".
    //    `last()` é seguro: a ordem não está vazia (item 1 retornou cedo se estivesse).
    if let Some(nome_piso) = config.ordem_fallback.last() {
        // Se o piso nem existe em 'provedores', o item 2 já emitiu erro; aqui só refinamos
        // quando ele existe.
        if let Some(piso) = config.provedor(nome_piso) {
            if !piso.habilitado {
                achados.push(Achado::erro(format!(
                    "piso '{nome_piso}' (último da ordem) está DESABILITADO: se toda a cadeia falhar, o robô fica mudo"
                )));
            }
            if tipo_exige_chave(&piso.tipo) {
                achados.push(Achado::erro(format!(
                    "piso '{nome_piso}' é do tipo '{}', que EXIGE chave externa: o piso não pode depender de chave/cota — use o Ollama local como último da ordem",
                    piso.tipo
                )));
            } else if piso.tipo != "ollama" {
                achados.push(Achado::aviso(format!(
                    "piso '{nome_piso}' é do tipo '{}', não 'ollama': o piso deveria ser o modelo local, que nunca depende de rede/chave externa e por isso nunca fica mudo",
                    piso.tipo
                )));
            }
        }
    }

    achados
}

/// `true` se algum achado é de gravidade [`Severidade::Erro`]. O binário usa isto para
/// decidir o código de saída (erro => processo falha, útil em cron/CI).
pub fn tem_erro(achados: &[Achado]) -> bool {
    achados.iter().any(|a| a.severidade == Severidade::Erro)
}

/// Conta quantos erros e quantos avisos há, para um resumo legível. Devolve `(erros, avisos)`.
pub fn contar(achados: &[Achado]) -> (usize, usize) {
    let erros = achados
        .iter()
        .filter(|a| a.severidade == Severidade::Erro)
        .count();
    (erros, achados.len() - erros)
}

#[cfg(test)]
mod testes {
    use super::*;
    use crate::config::interpretar;

    /// Atalho: parseia um JSON de config e roda a verificação.
    fn verificar_json(bruto: &str) -> Vec<Achado> {
        verificar(&interpretar(bruto).unwrap())
    }

    /// Config saudável (piso Ollama local, habilitado, com url_base+modelo): zero achados.
    #[test]
    fn config_saudavel_nao_gera_achado() {
        let achados = verificar_json(
            r#"{
                "ordem_fallback": ["claude", "ollama_local"],
                "provedores": {
                    "claude": {"tipo":"claude_cli","comando":"claude"},
                    "ollama_local": {"tipo":"ollama","url_base":"http://127.0.0.1:11434","modelo":"qwen2.5:1.5b"}
                }
            }"#,
        );
        assert!(
            achados.is_empty(),
            "esperava zero achados, veio: {achados:?}"
        );
        assert!(!tem_erro(&achados));
    }

    #[test]
    fn ordem_vazia_e_erro() {
        let achados = verificar_json(r#"{"ordem_fallback":[],"provedores":{}}"#);
        assert!(tem_erro(&achados));
        assert_eq!(achados.len(), 1);
    }

    #[test]
    fn nome_na_ordem_sem_provedor_e_erro() {
        let achados = verificar_json(
            r#"{"ordem_fallback":["fantasma","ollama_local"],
                "provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://x","modelo":"m"}}}"#,
        );
        assert!(tem_erro(&achados));
        assert!(
            achados
                .iter()
                .any(|a| a.mensagem.contains("'fantasma'")
                    && a.mensagem.contains("não foi declarado"))
        );
    }

    #[test]
    fn nome_duplicado_na_ordem_e_aviso() {
        let achados = verificar_json(
            r#"{"ordem_fallback":["ollama_local","ollama_local"],
                "provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://x","modelo":"m"}}}"#,
        );
        // Redundância é aviso, não erro (a config ainda roteia).
        assert!(!tem_erro(&achados));
        assert!(achados.iter().any(|a| a.mensagem.contains("2 vezes")));
    }

    #[test]
    fn provedor_fora_da_ordem_e_aviso() {
        let achados = verificar_json(
            r#"{"ordem_fallback":["ollama_local"],
                "provedores":{
                    "ollama_local":{"tipo":"ollama","url_base":"http://x","modelo":"m"},
                    "sobrando":{"tipo":"claude_cli"}
                }}"#,
        );
        assert!(!tem_erro(&achados));
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("'sobrando'") && a.mensagem.contains("nunca será usado")));
    }

    #[test]
    fn piso_desabilitado_e_erro() {
        let achados = verificar_json(
            r#"{"ordem_fallback":["ollama_local"],
                "provedores":{"ollama_local":{"tipo":"ollama","url_base":"http://x","modelo":"m","habilitado":false}}}"#,
        );
        assert!(tem_erro(&achados));
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("DESABILITADO") && a.mensagem.contains("mudo")));
    }

    #[test]
    fn piso_que_exige_chave_e_erro() {
        // Groq (openai_compat) como ÚLTIMO da ordem: depende de chave/cota → não serve de piso.
        let achados = verificar_json(
            r#"{"ordem_fallback":["groq"],
                "provedores":{"groq":{"tipo":"openai_compat","url_base":"http://x","modelo":"m","chave":"k","habilitado":true}}}"#,
        );
        assert!(tem_erro(&achados));
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("EXIGE chave") && a.mensagem.contains("piso")));
    }

    #[test]
    fn piso_claude_e_aviso_nao_erro() {
        // Claude como piso não exige chave (usa OAuth), mas PODE cair (token) → aviso.
        let achados = verificar_json(
            r#"{"ordem_fallback":["claude"],"provedores":{"claude":{"tipo":"claude_cli"}}}"#,
        );
        assert!(!tem_erro(&achados));
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("piso") && a.mensagem.contains("não 'ollama'")));
    }

    #[test]
    fn tipo_desconhecido_e_erro() {
        let achados = verificar_json(
            r#"{"ordem_fallback":["ollama_local","x"],
                "provedores":{
                    "ollama_local":{"tipo":"ollama","url_base":"http://x","modelo":"m"},
                    "x":{"tipo":"inventado"}
                }}"#,
        );
        assert!(tem_erro(&achados));
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("tipo 'inventado' desconhecido")));
    }

    #[test]
    fn ollama_sem_url_ou_modelo_e_erro() {
        let achados = verificar_json(
            r#"{"ordem_fallback":["ollama_local"],
                "provedores":{"ollama_local":{"tipo":"ollama"}}}"#,
        );
        assert!(tem_erro(&achados));
        // Dois erros de campo (url_base e modelo) + nada de piso (é ollama habilitado).
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("sem 'url_base'")));
        assert!(achados.iter().any(|a| a.mensagem.contains("sem 'modelo'")));
    }

    #[test]
    fn habilitado_sem_chave_e_aviso() {
        // Groq habilitado sem chave, MAS não é o piso (Ollama é) → só aviso, sem erro.
        let achados = verificar_json(
            r#"{"ordem_fallback":["groq","ollama_local"],
                "provedores":{
                    "groq":{"tipo":"openai_compat","url_base":"http://x","modelo":"m","habilitado":true},
                    "ollama_local":{"tipo":"ollama","url_base":"http://y","modelo":"m"}
                }}"#,
        );
        assert!(!tem_erro(&achados));
        assert!(achados
            .iter()
            .any(|a| a.mensagem.contains("'groq'") && a.mensagem.contains("sem 'chave'")));
    }

    #[test]
    fn contar_separa_erros_e_avisos() {
        // 1 erro (piso desabilitado) + 1 aviso (piso não-ollama? não: é ollama). Montamos
        // um caso com exatamente 1 erro e 1 aviso: piso ollama desabilitado (erro) e um
        // provedor sobrando (aviso).
        let achados = verificar_json(
            r#"{"ordem_fallback":["ollama_local"],
                "provedores":{
                    "ollama_local":{"tipo":"ollama","url_base":"http://x","modelo":"m","habilitado":false},
                    "sobrando":{"tipo":"claude_cli"}
                }}"#,
        );
        let (erros, avisos) = contar(&achados);
        assert_eq!(erros, 1, "achados: {achados:?}");
        assert_eq!(avisos, 1, "achados: {achados:?}");
    }
}
