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
pub mod disjuntor;
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
pub mod verificacao;

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

    // Disjuntor (circuit breaker): quando LIGADO na config, pulamos provedores que vêm
    // falhando em série, sem pagar a latência deles a cada mensagem. Desligado (padrão),
    // nada disto roda — nem lemos o arquivo — e o roteamento é idêntico ao de antes.
    let usar_disjuntor = config.disjuntor.habilitado;
    let agora = instante_epoch_segundos();
    let mut estado_disjuntor = if usar_disjuntor {
        disjuntor::EstadoDisjuntor::carregar(&config.disjuntor.caminho_estado)
    } else {
        disjuntor::EstadoDisjuntor::vazio()
    };
    // O piso (último da ordem) NUNCA é pulado pelo disjuntor: garante que o robô nunca
    // fica mudo mesmo com todos os circuitos de cima abertos.
    let indice_piso = config.ordem_fallback.len() - 1;

    for (indice, nome) in config.ordem_fallback.iter().enumerate() {
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

        // Disjuntor: se o circuito deste provedor está ABERTO (vem falhando em série) e ele
        // NÃO é o piso, pula sem gastar rede/processo — é justamente a latência que o
        // disjuntor economiza quando um provedor de cima está fora do ar.
        if usar_disjuntor && indice != indice_piso && estado_disjuntor.esta_aberto(nome, agora) {
            let falhas = estado_disjuntor.falhas_de(nome);
            let motivo = format!("{nome}: disjuntor aberto ({falhas} falhas seguidas) — pulando");
            telemetria::registrar(&format!("[disjuntor] {motivo}"));
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
                // Sucesso fecha o circuito (provedor voltou a si) e persiste o estado.
                if usar_disjuntor {
                    estado_disjuntor.apos_sucesso(nome);
                    estado_disjuntor.salvar(&config.disjuntor.caminho_estado);
                }
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
                // Registra a falha no disjuntor; se cruzar o limiar, abre o circuito.
                if usar_disjuntor {
                    estado_disjuntor.apos_falha(nome, agora, &config.disjuntor);
                }
                motivos.push(motivo);
            }
        }
    }

    // Persiste o estado do disjuntor após a cadeia (as falhas acumuladas acima). No
    // caminho de sucesso já salvamos e retornamos antes de chegar aqui.
    if usar_disjuntor {
        estado_disjuntor.salvar(&config.disjuntor.caminho_estado);
    }

    if !algum_provedor_construido {
        return Err(ErroRoteador::SemProvedores);
    }
    Err(ErroRoteador::TodosFalharam(motivos))
}

/// Instante atual em epoch (segundos UTC). Usado pelo disjuntor para medir o cooldown.
/// Isolado numa função para o resto do fluxo permanecer testável com instantes fixos.
fn instante_epoch_segundos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
            disjuntor: Default::default(),
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

    #[test]
    fn disjuntor_aberto_pula_o_topo_sem_tentar() {
        use crate::disjuntor::EstadoDisjuntor;

        // Arquivo de estado temporário com o 'topo' já ABERTO (falhando em série).
        let caminho = std::env::temp_dir().join("roteador-lib-disjuntor-teste.estado");
        let caminho = caminho.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&caminho);
        let mut estado = EstadoDisjuntor::vazio();
        let cfg_dj = crate::config::ConfigDisjuntor {
            habilitado: true,
            limiar_falhas: 3,
            cooldown_segundos: 100_000,
            caminho_estado: caminho.clone(),
        };
        // Ancoramos as falhas no AGORA real (o `rotear` usa o relógio real), senão o
        // `aberto_ate` cairia no passado e o circuito já estaria fechado quando testasse.
        let agora_real = instante_epoch_segundos();
        for _ in 0..3 {
            estado.apos_falha("topo", agora_real, &cfg_dj);
        }
        estado.salvar(&caminho);

        // Ordem [topo, piso]: os dois apontam para portas mortas (falham rápido), mas o
        // 'topo' deve ser PULADO pelo disjuntor (nem tenta a rede). O piso (último) NUNCA
        // é pulado, então ele é tentado e falha — provando que a cadeia andou pelo disjuntor.
        let json = format!(
            r#"{{
                "ordem_fallback": ["topo", "piso"],
                "provedores": {{
                    "topo": {{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}},
                    "piso": {{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}}
                }},
                "disjuntor": {{"habilitado": true, "cooldown_segundos": 100000, "caminho_estado": "{caminho}"}}
            }}"#
        );
        let config = interpretar(&json).unwrap();

        match rotear("oi", &Contexto::vazio(), &config).unwrap_err() {
            ErroRoteador::TodosFalharam(motivos) => {
                assert_eq!(motivos.len(), 2, "topo pulado + piso tentado");
                assert!(
                    motivos[0].contains("disjuntor aberto"),
                    "topo devia ser pulado pelo disjuntor, veio: {}",
                    motivos[0]
                );
                assert!(
                    motivos[1].starts_with("piso:") && !motivos[1].contains("disjuntor"),
                    "piso nunca é pulado pelo disjuntor, veio: {}",
                    motivos[1]
                );
            }
            outro => panic!("esperava TodosFalharam, veio {outro:?}"),
        }

        let _ = std::fs::remove_file(&caminho);
    }
}
