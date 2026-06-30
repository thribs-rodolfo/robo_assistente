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

/// Monta um prompt em texto único: usado por provedores que recebem um campo `prompt`
/// (Ollama) ou um único bloco de texto (Gemini). Concatena sistema + histórico + mensagem.
pub fn montar_prompt(mensagem: &str, contexto: &Contexto) -> String {
    let mut partes: Vec<String> = Vec::new();
    if let Some(sistema) = &contexto.sistema {
        partes.push(sistema.clone());
    }
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
