//! Montagem do prompt a partir de (mensagem, contexto).
//!
//! O `Contexto` é agnóstico de provedor: carrega uma instrução de sistema opcional e um
//! histórico de turnos. Cada provedor decide como transformar isso no formato que sua API
//! espera — uns querem um texto único (Ollama, Gemini), outros uma lista de mensagens
//! com papéis (APIs estilo OpenAI). Por isso oferecemos as duas montagens aqui.

/// Quem falou em um turno do histórico.
#[derive(Debug, Clone, PartialEq)]
pub enum Autor {
    Usuario,
    Assistente,
}

/// Um turno de conversa já ocorrido (para dar memória curta ao modelo).
#[derive(Debug, Clone, PartialEq)]
pub struct Turno {
    pub autor: Autor,
    pub texto: String,
}

/// Contexto de uma chamada: instrução de sistema opcional + histórico.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Contexto {
    pub sistema: Option<String>,
    pub historico: Vec<Turno>,
}

impl Contexto {
    /// Contexto vazio (sem sistema, sem histórico) — atalho legível.
    pub fn vazio() -> Self {
        Contexto::default()
    }
}

/// Monta só a CONVERSA (histórico + mensagem atual), SEM a instrução de sistema.
///
/// Serve aos provedores que têm um campo DEDICADO para a persona/sistema — o Ollama
/// `/api/generate` aceita um `system` próprio, separado do `prompt`. Passar a persona por
/// esse campo (em vez de amassá-la junto com os turnos do usuário) melhora a aderência do
/// modelo à instrução, o que importa especialmente no piso (qwen2.5:1.5b, modelo fraco no
/// qual repousa a garantia "o robô nunca fica mudo"). O [`montar_prompt`] reusa isto e só
/// prefixa o sistema, para os provedores que recebem um bloco de texto ÚNICO (Gemini).
pub fn montar_conversa(mensagem: &str, contexto: &Contexto) -> String {
    let mut partes: Vec<String> = Vec::new();
    for turno in &contexto.historico {
        let rotulo = match turno.autor {
            Autor::Usuario => "usuario",
            Autor::Assistente => "assistente",
        };
        partes.push(format!("{rotulo}: {}", turno.texto));
    }
    partes.push(format!("usuario: {mensagem}"));
    partes.join("\n\n")
}

/// Monta um prompt em texto único: usado por provedores que recebem um bloco de texto só
/// (Gemini). Concatena sistema + [`montar_conversa`] (histórico + mensagem).
pub fn montar_prompt(mensagem: &str, contexto: &Contexto) -> String {
    let conversa = montar_conversa(mensagem, contexto);
    match &contexto.sistema {
        Some(sistema) => format!("{sistema}\n\n{conversa}"),
        None => conversa,
    }
}

/// Monta a CONVERSA como uma lista de turnos com autor (histórico + mensagem atual),
/// SEM a instrução de sistema.
///
/// Serve aos provedores que modelam a conversa como uma SEQUÊNCIA de turnos com papel
/// dedicado E têm um campo próprio para a persona — o Gemini `generateContent` usa
/// `contents` (lista de turnos com `role` user/model) + `systemInstruction` à parte. Assim
/// o modelo distingue quem falou o quê, em vez de receber tudo amassado num texto só. Cada
/// provedor mapeia [`Autor`] para o rótulo que sua API espera (Gemini: Usuario→"user",
/// Assistente→"model"). A persona NÃO entra aqui (vai pelo campo dedicado do provedor).
pub fn montar_turnos(mensagem: &str, contexto: &Contexto) -> Vec<Turno> {
    let mut turnos = contexto.historico.clone();
    turnos.push(Turno {
        autor: Autor::Usuario,
        texto: mensagem.to_string(),
    });
    turnos
}

/// Um par (papel, conteúdo) no formato de mensagens estilo OpenAI/Chat.
#[derive(Debug, Clone, PartialEq)]
pub struct MensagemChat {
    pub papel: String,
    pub conteudo: String,
}

/// Monta a lista de mensagens com papéis (system/user/assistant), usada por APIs
/// compatíveis com OpenAI (ex.: Groq).
pub fn montar_mensagens(mensagem: &str, contexto: &Contexto) -> Vec<MensagemChat> {
    let mut mensagens: Vec<MensagemChat> = Vec::new();
    if let Some(sistema) = &contexto.sistema {
        mensagens.push(MensagemChat {
            papel: "system".into(),
            conteudo: sistema.clone(),
        });
    }
    for turno in &contexto.historico {
        let papel = match turno.autor {
            Autor::Usuario => "user",
            Autor::Assistente => "assistant",
        };
        mensagens.push(MensagemChat {
            papel: papel.into(),
            conteudo: turno.texto.clone(),
        });
    }
    mensagens.push(MensagemChat {
        papel: "user".into(),
        conteudo: mensagem.to_string(),
    });
    mensagens
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn prompt_inclui_sistema_historico_e_mensagem() {
        let contexto = Contexto {
            sistema: Some("Você é o Rodolfo.".into()),
            historico: vec![Turno {
                autor: Autor::Usuario,
                texto: "oi".into(),
            }],
        };
        let prompt = montar_prompt("tudo bem?", &contexto);
        assert!(prompt.contains("Você é o Rodolfo."));
        assert!(prompt.contains("usuario: oi"));
        assert!(prompt.contains("usuario: tudo bem?"));
    }

    #[test]
    fn conversa_nao_inclui_o_sistema() {
        // A conversa carrega só histórico + mensagem; a persona vai pelo campo dedicado do
        // provedor (Ollama `system`), então NÃO deve aparecer aqui — senão duplicaria.
        let contexto = Contexto {
            sistema: Some("Você é o Rodolfo.".into()),
            historico: vec![Turno {
                autor: Autor::Assistente,
                texto: "olá".into(),
            }],
        };
        let conversa = montar_conversa("tudo bem?", &contexto);
        assert!(!conversa.contains("Você é o Rodolfo."));
        assert!(conversa.contains("assistente: olá"));
        assert!(conversa.contains("usuario: tudo bem?"));
    }

    #[test]
    fn prompt_com_sistema_prefixa_a_conversa_sem_duplicar() {
        // O prompt de texto único (Gemini) é sistema + conversa, com o sistema aparecendo
        // UMA vez só (no início) e a conversa logo em seguida.
        let contexto = Contexto {
            sistema: Some("SIS".into()),
            historico: vec![],
        };
        let prompt = montar_prompt("oi", &contexto);
        assert_eq!(prompt, "SIS\n\nusuario: oi");
        assert_eq!(prompt.matches("SIS").count(), 1);
    }

    #[test]
    fn prompt_sem_sistema_e_so_a_conversa() {
        let contexto = Contexto::vazio();
        assert_eq!(montar_prompt("oi", &contexto), "usuario: oi");
    }

    #[test]
    fn turnos_carregam_historico_mais_mensagem_sem_o_sistema() {
        // montar_turnos NÃO inclui a persona (vai pelo campo dedicado do provedor) e
        // termina sempre na mensagem atual, marcada como do usuário.
        let contexto = Contexto {
            sistema: Some("PERSONA".into()),
            historico: vec![
                Turno {
                    autor: Autor::Usuario,
                    texto: "oi".into(),
                },
                Turno {
                    autor: Autor::Assistente,
                    texto: "olá".into(),
                },
            ],
        };
        let turnos = montar_turnos("tudo bem?", &contexto);
        assert_eq!(turnos.len(), 3);
        assert_eq!(turnos[0].autor, Autor::Usuario);
        assert_eq!(turnos[0].texto, "oi");
        assert_eq!(turnos[1].autor, Autor::Assistente);
        assert_eq!(turnos[2].autor, Autor::Usuario);
        assert_eq!(turnos[2].texto, "tudo bem?");
        // A persona não aparece em nenhum turno.
        assert!(turnos.iter().all(|t| t.texto != "PERSONA"));
    }

    #[test]
    fn mensagens_mapeiam_papeis_corretos() {
        let contexto = Contexto {
            sistema: Some("sis".into()),
            historico: vec![
                Turno {
                    autor: Autor::Usuario,
                    texto: "u1".into(),
                },
                Turno {
                    autor: Autor::Assistente,
                    texto: "a1".into(),
                },
            ],
        };
        let msgs = montar_mensagens("u2", &contexto);
        assert_eq!(msgs[0].papel, "system");
        assert_eq!(msgs[1].papel, "user");
        assert_eq!(msgs[2].papel, "assistant");
        assert_eq!(msgs[3].papel, "user");
        assert_eq!(msgs[3].conteudo, "u2");
    }
}
