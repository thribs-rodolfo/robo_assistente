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
pub mod arquivo;
pub mod config;
pub mod diagnostico;
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
pub mod retentativa;
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
                telemetria::registrar_em(&config.telemetria_log, &motivo);
                motivos.push(motivo);
                continue;
            }
        };

        // Constrói a implementação concreta a partir do `tipo`.
        let provedor = match provedor::construir(config_provedor) {
            Some(p) => p,
            None => {
                let motivo = format!("{nome}: tipo '{}' desconhecido", config_provedor.tipo);
                telemetria::registrar_em(&config.telemetria_log, &motivo);
                motivos.push(motivo);
                continue;
            }
        };
        algum_provedor_construido = true;

        // Pré-checagem: desabilitado ou sem chave? Pula sem gastar rede.
        if let Err(falha) = provedor.disponivel() {
            let motivo = format!("{nome}: {falha}");
            telemetria::registrar_em(&config.telemetria_log, &format!("[pula] {motivo}"));
            motivos.push(motivo);
            continue;
        }

        // Disjuntor: o circuito deste provedor está ABERTO (vem falhando em série) e ele NÃO é
        // o piso? O piso nunca é pulado — garante que o robô jamais fica mudo.
        let circuito_aberto =
            usar_disjuntor && indice != indice_piso && estado_disjuntor.esta_aberto(nome, agora);

        // MODO ATIVO (sombra desligada): pula sem gastar rede/processo — é justamente a
        // latência que o disjuntor economiza quando um provedor de cima está fora do ar.
        if circuito_aberto && !config.disjuntor.sombra {
            let falhas = estado_disjuntor.falhas_de(nome);
            let motivo = format!("{nome}: disjuntor aberto ({falhas} falhas seguidas) — pulando");
            telemetria::registrar_em(&config.telemetria_log, &format!("[disjuntor] {motivo}"));
            motivos.push(motivo);
            continue;
        }
        // MODO SOMBRA (`sombra: true`): se o circuito está aberto, NÃO pulamos — seguimos e
        // tentamos o provedor de verdade (roteamento idêntico ao de hoje). O flag
        // `circuito_aberto` fica guardado; depois da tentativa (nos ramos Ok/Err abaixo)
        // comparamos "o que o disjuntor ATIVO faria" com o que REALMENTE aconteceu.

        // Tentativa real, com RE-tentativas em falhas TRANSITÓRIAS (blip de rede, 429, 5xx)
        // antes de cair pro próximo. Retentar no provedor bom evita jogar o robô no piso por
        // uma falha passageira. Com `retentativas: 0` (padrão), é uma tentativa só = o
        // comportamento antigo, sem custo de latência extra. Medimos a latência de TODA a
        // sequência (tentativa + retentativas) para a telemetria de custo/performance.
        let inicio = std::time::Instant::now();
        let resultado = tentar_com_retentativas(
            provedor.as_ref(),
            mensagem,
            contexto,
            config_provedor,
            &config.telemetria_log,
        );
        let ms = inicio.elapsed().as_millis();
        match resultado {
            Ok(texto) => {
                telemetria::registrar_em(
                    &config.telemetria_log,
                    &format!("[ok] respondido por '{nome}' em {ms}ms"),
                );
                // Sombra: o disjuntor ATIVO teria PULADO este provedor, mas ele RESPONDEU. É um
                // FALSO POSITIVO — ligar o disjuntor agora custaria esta resposta boa. Sinal de
                // ouro para o Thiago decidir/afinar (subir limiar/cooldown) antes de ativar.
                if circuito_aberto {
                    let falhas = estado_disjuntor.falhas_de(nome);
                    telemetria::registrar_em(
                        &config.telemetria_log,
                        &format!(
                            "[disjuntor-sombra] PULARIA '{nome}' (circuito aberto, {falhas} falhas seguidas) mas ele RESPONDEU em {ms}ms — FALSO POSITIVO (não ligar ainda / afinar limiar)"
                        ),
                    );
                }
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
                telemetria::registrar_em(
                    &config.telemetria_log,
                    &format!("[falha] {motivo} (após {ms}ms) — caindo pro próximo"),
                );
                // Sombra: o disjuntor ATIVO teria pulado, e o provedor de fato FALHOU — previsão
                // CERTA. A latência que ele acabou de gastar ({ms}ms) é exatamente o que o
                // disjuntor ligado teria economizado nesta mensagem. É a economia virando número.
                if circuito_aberto {
                    let falhas = estado_disjuntor.falhas_de(nome);
                    telemetria::registrar_em(
                        &config.telemetria_log,
                        &format!(
                            "[disjuntor-sombra] pularia '{nome}' (circuito aberto, {falhas} falhas seguidas) e teria economizado ~{ms}ms — ele falhou como previsto"
                        ),
                    );
                }
                // Registra a falha no disjuntor SÓ se ela indicar que o PROVEDOR está
                // indisponível (rede/timeout/processo/401/429/5xx). Falha específica da
                // mensagem (HTTP 400/404/413/422, resposta vazia/inválida) NÃO abre o
                // circuito: puniria um provedor são, jogando o robô no piso à toa — o
                // oposto do objetivo (depender MENOS do piso). Ver
                // FalhaProvedor::indica_provedor_indisponivel.
                if usar_disjuntor {
                    if falha.indica_provedor_indisponivel() {
                        estado_disjuntor.apos_falha(nome, agora, &config.disjuntor);
                    } else {
                        // Deixa o contador de falhas seguidas intacto (nem soma, nem zera):
                        // esta falha não diz nada sobre a saúde do provedor. Só registra,
                        // sem prefixo de métrica, para não contaminar a contagem do bin/metricas.
                        telemetria::registrar_em(
                            &config.telemetria_log,
                            &format!(
                                "[roteamento] {nome}: falha da mensagem — não conta pro disjuntor"
                            ),
                        );
                    }
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

/// Tenta um provedor, RE-tentando em falhas transitórias antes de desistir.
///
/// Chama `responder`; se falhar, pergunta a [`retentativa::planejar`] se vale retentar (e
/// quanto esperar). Se sim, registra `[retentativa]` na telemetria, dorme o backoff e tenta
/// de novo; se não (orçamento esgotado ou falha não-transitória), devolve a falha para o
/// [`rotear`] cair para o próximo provedor.
///
/// A decisão de retentar é pura (testada em [`crate::retentativa`]); aqui fica só o efeito
/// colateral (dormir + logar), fino e direto.
fn tentar_com_retentativas(
    provedor: &dyn Provedor,
    mensagem: &str,
    contexto: &Contexto,
    config_provedor: &config::ConfigProvedor,
    telemetria_log: &str,
) -> Result<String, FalhaProvedor> {
    let nome = &config_provedor.nome;
    let mut tentativa: u32 = 0;
    loop {
        match provedor.responder(mensagem, contexto) {
            Ok(texto) => return Ok(texto),
            Err(falha) => match retentativa::planejar(
                &falha,
                tentativa,
                config_provedor.retentativas,
                config_provedor.retentativa_espera_ms,
            ) {
                Some(espera) => {
                    telemetria::registrar_em(
                        telemetria_log,
                        &format!(
                            "[retentativa] {nome}: {falha} — retentando ({} de {}) após {}ms",
                            tentativa + 1,
                            config_provedor.retentativas,
                            espera.as_millis()
                        ),
                    );
                    std::thread::sleep(espera);
                    tentativa += 1;
                }
                None => return Err(falha),
            },
        }
    }
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
    use std::cell::RefCell;
    use std::time::Duration;

    /// Provedor de mentira para testar a retentativa: devolve, em ordem, as falhas da lista
    /// `falhas_pendentes`; quando a lista esvazia, responde "ok". Conta quantas vezes foi
    /// chamado, para provarmos que a retentativa tentou (ou NÃO tentou) o número certo de vezes.
    struct ProvedorFalso {
        falhas_pendentes: RefCell<Vec<FalhaProvedor>>,
        chamadas: RefCell<u32>,
    }

    impl ProvedorFalso {
        fn com(falhas: Vec<FalhaProvedor>) -> Self {
            ProvedorFalso {
                falhas_pendentes: RefCell::new(falhas),
                chamadas: RefCell::new(0),
            }
        }
    }

    impl Provedor for ProvedorFalso {
        fn nome(&self) -> &str {
            "falso"
        }
        fn disponivel(&self) -> Result<(), FalhaProvedor> {
            Ok(())
        }
        fn responder(&self, _m: &str, _c: &Contexto) -> Result<String, FalhaProvedor> {
            *self.chamadas.borrow_mut() += 1;
            let mut pendentes = self.falhas_pendentes.borrow_mut();
            if pendentes.is_empty() {
                Ok("resposta boa".into())
            } else {
                Err(pendentes.remove(0))
            }
        }
    }

    /// ConfigProvedor mínima para os testes de retentativa (espera curta para não travar).
    fn config_com_retentativas(retentativas: u32) -> config::ConfigProvedor {
        config::ConfigProvedor {
            nome: "falso".into(),
            tipo: "ollama".into(),
            url_base: None,
            modelo: None,
            comando: None,
            chave: None,
            timeout: Duration::from_secs(1),
            habilitado: true,
            retentativas,
            retentativa_espera_ms: 1,
        }
    }

    const LOG_RETENTATIVA: &str = "/tmp/roteador-testes-retentativa.log";

    #[test]
    fn retenta_falha_transitoria_e_acaba_respondendo() {
        // Dois blips de rede seguidos, com orçamento 2 -> deve retentar e vencer no fim.
        let provedor = ProvedorFalso::com(vec![
            FalhaProvedor::Rede("piscou".into()),
            FalhaProvedor::Rede("piscou de novo".into()),
        ]);
        let cfg = config_com_retentativas(2);
        let r = tentar_com_retentativas(&provedor, "oi", &Contexto::vazio(), &cfg, LOG_RETENTATIVA);
        assert_eq!(r.unwrap(), "resposta boa");
        assert_eq!(
            *provedor.chamadas.borrow(),
            3,
            "1 tentativa + 2 retentativas"
        );
    }

    #[test]
    fn desiste_quando_estoura_o_orcamento() {
        // Três blips, mas orçamento só 1 -> não chega ao sucesso; devolve a falha.
        let provedor = ProvedorFalso::com(vec![
            FalhaProvedor::Rede("1".into()),
            FalhaProvedor::Rede("2".into()),
            FalhaProvedor::Rede("3".into()),
        ]);
        let cfg = config_com_retentativas(1);
        let r = tentar_com_retentativas(&provedor, "oi", &Contexto::vazio(), &cfg, LOG_RETENTATIVA);
        assert!(r.is_err());
        assert_eq!(
            *provedor.chamadas.borrow(),
            2,
            "1 tentativa + 1 retentativa"
        );
    }

    #[test]
    fn nao_retenta_falha_nao_transitoria() {
        // Processo (Claude CLI): não deve retentar mesmo com orçamento alto (nem martelar).
        let provedor = ProvedorFalso::com(vec![FalhaProvedor::Processo("token caiu".into())]);
        let cfg = config_com_retentativas(5);
        let r = tentar_com_retentativas(&provedor, "oi", &Contexto::vazio(), &cfg, LOG_RETENTATIVA);
        assert!(r.is_err());
        assert_eq!(
            *provedor.chamadas.borrow(),
            1,
            "sem retentativa em Processo"
        );
    }

    #[test]
    fn sem_orcamento_e_uma_tentativa_so() {
        // Padrão do projeto (retentativas: 0): comportamento antigo, uma tentativa.
        let provedor = ProvedorFalso::com(vec![FalhaProvedor::Rede("x".into())]);
        let cfg = config_com_retentativas(0);
        let r = tentar_com_retentativas(&provedor, "oi", &Contexto::vazio(), &cfg, LOG_RETENTATIVA);
        assert!(r.is_err());
        assert_eq!(*provedor.chamadas.borrow(), 1);
    }

    /// Log temporário para os testes: mantém o roteamento HERMÉTICO e NÃO suja o log de
    /// produção (a fonte das métricas do `bin/metricas`). Antes, sem isto, cada `cargo test`
    /// gravava `[falha]`/`[ok]` de porta-morta no log real e contaminava a medida "% no piso".
    const LOG_TESTE: &str = "/tmp/roteador-testes-lib.log";

    #[test]
    fn ordem_vazia_da_erro() {
        let config = Config {
            ordem_fallback: vec![],
            provedores: vec![],
            disjuntor: Default::default(),
            telemetria_log: LOG_TESTE.to_string(),
        };
        let erro = rotear("oi", &Contexto::vazio(), &config).unwrap_err();
        assert_eq!(erro, ErroRoteador::SemProvedores);
    }

    #[test]
    fn provedor_na_ordem_sem_config_e_pulado_e_falha_no_fim() {
        // 'fantasma' está na ordem mas não nos provedores; nenhum provedor é construído.
        let config = interpretar(
            r#"{"ordem_fallback":["fantasma"],"provedores":{},"telemetria_log":"/tmp/roteador-testes-lib.log"}"#,
        )
        .unwrap();
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
                "telemetria_log": "/tmp/roteador-testes-lib.log",
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
            sombra: false,
            limiar_falhas: 3,
            cooldown_segundos: 100_000,
            cooldown_maximo_segundos: 1_000_000,
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
                "disjuntor": {{"habilitado": true, "cooldown_segundos": 100000, "caminho_estado": "{caminho}"}},
                "telemetria_log": "/tmp/roteador-testes-lib.log"
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

    #[test]
    fn modo_sombra_nao_pula_mas_registra_o_que_faria() {
        use crate::disjuntor::EstadoDisjuntor;

        // Estado com o 'topo' já ABERTO (falhando em série), igual ao teste do modo ativo.
        let caminho = std::env::temp_dir().join("roteador-lib-sombra-teste.estado");
        let caminho = caminho.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&caminho);
        let mut estado = EstadoDisjuntor::vazio();
        let cfg_dj = crate::config::ConfigDisjuntor {
            habilitado: true,
            sombra: true,
            limiar_falhas: 3,
            cooldown_segundos: 100_000,
            cooldown_maximo_segundos: 1_000_000,
            caminho_estado: caminho.clone(),
        };
        let agora_real = instante_epoch_segundos();
        for _ in 0..3 {
            estado.apos_falha("topo", agora_real, &cfg_dj);
        }
        estado.salvar(&caminho);

        // Log de telemetria próprio (temporário) para inspecionar as linhas [disjuntor-sombra].
        let log = std::env::temp_dir().join("roteador-lib-sombra.log");
        let log = log.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&log);

        // Mesma cadeia [topo, piso] em portas mortas, MAS com `sombra: true`. Diferença crucial
        // vs. o modo ativo: o 'topo' NÃO é pulado — é tentado (e falha, porta morta).
        let json = format!(
            r#"{{
                "ordem_fallback": ["topo", "piso"],
                "provedores": {{
                    "topo": {{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}},
                    "piso": {{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}}
                }},
                "disjuntor": {{"habilitado": true, "sombra": true, "cooldown_segundos": 100000, "caminho_estado": "{caminho}"}},
                "telemetria_log": "{log}"
            }}"#
        );
        let config = interpretar(&json).unwrap();

        match rotear("oi", &Contexto::vazio(), &config).unwrap_err() {
            ErroRoteador::TodosFalharam(motivos) => {
                assert_eq!(motivos.len(), 2, "topo TENTADO (não pulado) + piso tentado");
                // O 'topo' foi tentado de verdade: o motivo é a falha real, NÃO "disjuntor pulando".
                assert!(
                    motivos[0].starts_with("topo:") && !motivos[0].contains("disjuntor aberto"),
                    "no modo sombra o topo é tentado, não pulado; veio: {}",
                    motivos[0]
                );
            }
            outro => panic!("esperava TodosFalharam, veio {outro:?}"),
        }

        // A telemetria deve conter a linha da SOMBRA dizendo que PULARIA o topo e a economia.
        let conteudo_log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            conteudo_log.contains("[disjuntor-sombra] pularia 'topo'"),
            "faltou a linha de sombra no log; veio:\n{conteudo_log}"
        );
        // E NÃO deve conter a linha do modo ATIVO ("[disjuntor] ... pulando"): não pulou de fato.
        assert!(
            !conteudo_log.contains("[disjuntor] topo: disjuntor aberto"),
            "sombra não pode emitir a linha de pulo real; veio:\n{conteudo_log}"
        );

        let _ = std::fs::remove_file(&caminho);
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn falha_de_rede_abre_o_disjuntor() {
        use crate::disjuntor::EstadoDisjuntor;

        // Prova a FIAÇÃO da classificação: uma falha de REDE (porta morta) indica que o
        // provedor está indisponível, então DEVE contar pro disjuntor e, com limiar 1,
        // abrir o circuito. (A classificação em si é testada em erro.rs; aqui provamos que
        // o `rotear` de fato só chama `apos_falha` para falhas de indisponibilidade.)
        let caminho = std::env::temp_dir().join("roteador-lib-rede-abre.estado");
        let caminho = caminho.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&caminho);

        // Cadeia [alvo, piso], os dois em porta morta -> falha de rede rápida. Disjuntor
        // ligado com limiar 1: uma única falha de rede no 'alvo' já abre o circuito dele.
        let json = format!(
            r#"{{
                "ordem_fallback": ["alvo", "piso"],
                "provedores": {{
                    "alvo": {{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}},
                    "piso": {{"tipo":"ollama","url_base":"http://127.0.0.1:1","modelo":"m","timeout_segundos":1}}
                }},
                "disjuntor": {{"habilitado": true, "limiar_falhas": 1, "cooldown_segundos": 100000, "caminho_estado": "{caminho}"}},
                "telemetria_log": "/tmp/roteador-testes-lib.log"
            }}"#
        );
        let config = interpretar(&json).unwrap();

        // Uma rodada: o 'alvo' falha por rede e deve abrir; o roteamento inteiro falha
        // (os dois em porta morta), o que é esperado — o que importa é o estado gravado.
        let _ = rotear("oi", &Contexto::vazio(), &config);

        let estado = EstadoDisjuntor::carregar(&caminho);
        assert!(
            estado.esta_aberto("alvo", instante_epoch_segundos()),
            "falha de rede devia contar e abrir o circuito do 'alvo'"
        );

        let _ = std::fs::remove_file(&caminho);
    }
}
