//! Os provedores e o trait que os torna intercambiáveis.
//!
//! O coração do "agnosticismo": o roteador não sabe se está falando com Ollama, Claude,
//! Groq ou Gemini. Ele só conhece o trait `Provedor`. Trocar/adicionar provedor é
//! implementar o trait e citar o nome na `ordem_fallback`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::ConfigProvedor;
use crate::erro::FalhaProvedor;
use crate::http;
use crate::json::{self, Valor};
use crate::prompt::{self, Contexto};

/// Interface comum a todo provedor de modelo. Agnóstico a canal e a plataforma.
pub trait Provedor {
    /// Nome lógico do provedor (para telemetria: saber quem respondeu).
    fn nome(&self) -> &str;

    /// Pré-checagem barata ANTES de gastar rede: está habilitado? tem chave?
    /// Se devolver `false`, o roteador pula direto para o próximo.
    fn disponivel(&self) -> Result<(), FalhaProvedor>;

    /// Tenta responder. Sucesso devolve o texto; falha recuperável devolve `FalhaProvedor`
    /// e o roteador cai para o próximo provedor da cadeia.
    fn responder(&self, mensagem: &str, contexto: &Contexto) -> Result<String, FalhaProvedor>;
}

/// Constrói um provedor concreto a partir da sua config, escolhendo pela string `tipo`.
/// Tipos desconhecidos viram `None` (o roteador apenas pula, com aviso na telemetria).
pub fn construir(config: &ConfigProvedor) -> Option<Box<dyn Provedor>> {
    match config.tipo.as_str() {
        "ollama" => Some(Box::new(ProvedorOllama {
            config: config.clone(),
        })),
        "claude_cli" => Some(Box::new(ProvedorClaudeCli {
            config: config.clone(),
        })),
        "openai_compat" => Some(Box::new(ProvedorOpenAiCompat {
            config: config.clone(),
        })),
        "gemini_rest" => Some(Box::new(ProvedorGeminiRest {
            config: config.clone(),
        })),
        _ => None,
    }
}

// --------------------------------------------------------------------------- //
// Ollama local — modelo na própria máquina. Piso de emergência: lento e fraco,
// mas custo zero e sempre vivo. Deve ser SEMPRE o último da ordem de fallback.
// --------------------------------------------------------------------------- //
struct ProvedorOllama {
    config: ConfigProvedor,
}

impl Provedor for ProvedorOllama {
    fn nome(&self) -> &str {
        &self.config.nome
    }

    fn disponivel(&self) -> Result<(), FalhaProvedor> {
        if !self.config.habilitado {
            return Err(FalhaProvedor::Indisponivel("desabilitado na config".into()));
        }
        Ok(())
    }

    fn responder(&self, mensagem: &str, contexto: &Contexto) -> Result<String, FalhaProvedor> {
        let url_base = self
            .config
            .url_base
            .as_deref()
            .ok_or_else(|| FalhaProvedor::Indisponivel("ollama sem 'url_base'".into()))?;
        let modelo = self
            .config
            .modelo
            .as_deref()
            .ok_or_else(|| FalhaProvedor::Indisponivel("ollama sem 'modelo'".into()))?;

        let url = format!("{}/api/generate", url_base.trim_end_matches('/'));

        // Monta o corpo {"model":..., "prompt":..., "stream": false} com nosso JSON próprio.
        let corpo = Valor::Objeto(vec![
            ("model".into(), Valor::Texto(modelo.to_string())),
            (
                "prompt".into(),
                Valor::Texto(prompt::montar_prompt(mensagem, contexto)),
            ),
            ("stream".into(), Valor::Booleano(false)),
        ])
        .para_texto();

        let resposta = http::post_json(&url, &corpo, &[], self.config.timeout)?;
        if resposta.status != 200 {
            return Err(FalhaProvedor::Http {
                status: resposta.status,
                corpo: resposta.corpo,
            });
        }

        // O Ollama responde {"response":"...","done":true,...}. Extraímos "response".
        let raiz = json::parsear(&resposta.corpo)
            .map_err(|e| FalhaProvedor::RespostaInvalida(e.to_string()))?;
        let texto = raiz
            .obter("response")
            .and_then(Valor::como_texto)
            .ok_or_else(|| FalhaProvedor::RespostaInvalida("sem campo 'response'".into()))?
            .trim()
            .to_string();
        if texto.is_empty() {
            return Err(FalhaProvedor::RespostaVazia);
        }
        Ok(texto)
    }
}

// --------------------------------------------------------------------------- //
// Claude via CLI (`claude --print`). Usa o token OAuth já instalado na máquina.
//
// CUIDADO (licao-refresh-token-rotativo): este provedor NUNCA chama o endpoint de
// refresh do Claude. Se o token caiu, o `claude --print` falha, viramos isso em
// FalhaProvedor e caímos para o próximo — sem tocar no refresh (quem rotaciona é só
// o cron de produção). Por isso, jamais disparamos o Claude "de propósito só pra testar".
// --------------------------------------------------------------------------- //
struct ProvedorClaudeCli {
    config: ConfigProvedor,
}

impl Provedor for ProvedorClaudeCli {
    fn nome(&self) -> &str {
        &self.config.nome
    }

    fn disponivel(&self) -> Result<(), FalhaProvedor> {
        if !self.config.habilitado {
            return Err(FalhaProvedor::Indisponivel("desabilitado na config".into()));
        }
        Ok(())
    }

    fn responder(&self, mensagem: &str, contexto: &Contexto) -> Result<String, FalhaProvedor> {
        let comando = self.config.comando.as_deref().unwrap_or("claude");
        let prompt_texto = prompt::montar_prompt(mensagem, contexto);

        // Sobe o processo com stdin/stdout em pipe para enviarmos o prompt e lermos a resposta.
        let mut filho = Command::new(comando)
            .arg("--print")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| FalhaProvedor::Processo(format!("não subiu '{comando}': {e}")))?;

        // Escreve o prompt no stdin e fecha (o `take` move o stdin pra fora do filho, e o
        // drop ao fim deste bloco fecha o pipe — sinaliza "fim da entrada" ao processo).
        {
            let stdin = filho
                .stdin
                .take()
                .ok_or_else(|| FalhaProvedor::Processo("sem stdin no processo".into()))?;
            let mut stdin = stdin;
            stdin
                .write_all(prompt_texto.as_bytes())
                .map_err(|e| FalhaProvedor::Processo(format!("falha ao escrever no stdin: {e}")))?;
        }

        // Espera com timeout próprio (a stdlib não tem wait com prazo): consultamos
        // `try_wait` em laço curto até o processo terminar ou estourar o tempo.
        let prazo = Instant::now() + self.config.timeout;
        loop {
            match filho.try_wait() {
                Ok(Some(_)) => break, // terminou
                Ok(None) => {
                    if Instant::now() >= prazo {
                        // Estourou: mata o processo para não deixar órfão e reporta a falha.
                        let _ = filho.kill();
                        let _ = filho.wait();
                        return Err(FalhaProvedor::Processo("estourou o timeout".into()));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(FalhaProvedor::Processo(format!("erro ao aguardar: {e}"))),
            }
        }

        // Coleta a saída completa (já terminou, então o read não bloqueia).
        let saida = filho
            .wait_with_output()
            .map_err(|e| FalhaProvedor::Processo(format!("falha ao coletar saída: {e}")))?;
        if !saida.status.success() {
            let erro = String::from_utf8_lossy(&saida.stderr);
            return Err(FalhaProvedor::Processo(format!(
                "código {:?}: {}",
                saida.status.code(),
                erro.trim()
            )));
        }
        let texto = String::from_utf8_lossy(&saida.stdout).trim().to_string();
        if texto.is_empty() {
            return Err(FalhaProvedor::RespostaVazia);
        }
        Ok(texto)
    }
}

// --------------------------------------------------------------------------- //
// Slots para provedores remotos via HTTPS: Groq (API estilo OpenAI) e Gemini (REST).
//
// Estado atual: estão DECLARADOS e desabilitados. A chamada real depende de um cliente
// HTTPS/TLS, que ainda não escrevemos (TLS à mão é inviável; decidiremos a abordagem —
// crate mínima como `ureq`, ou um túnel — em passo futuro). Por honestidade (sem erro
// silencioso), se alguém habilitar e cair aqui, devolvemos uma falha clara, nunca um
// sucesso falso. Estão bloqueados de fora também: Gemini sem cota, Groq sem chave.
// --------------------------------------------------------------------------- //
struct ProvedorOpenAiCompat {
    config: ConfigProvedor,
}

impl Provedor for ProvedorOpenAiCompat {
    fn nome(&self) -> &str {
        &self.config.nome
    }

    fn disponivel(&self) -> Result<(), FalhaProvedor> {
        if !self.config.habilitado {
            return Err(FalhaProvedor::Indisponivel("desabilitado na config".into()));
        }
        if self.config.chave.as_deref().unwrap_or("").is_empty() {
            return Err(FalhaProvedor::Indisponivel("sem chave de API".into()));
        }
        // Habilitado e com chave, mas ainda sem cliente HTTPS: avisa em vez de fingir.
        Err(FalhaProvedor::Indisponivel(
            "cliente HTTPS ainda não implementado (passo futuro) — Groq fica de fora por ora"
                .into(),
        ))
    }

    fn responder(&self, _mensagem: &str, _contexto: &Contexto) -> Result<String, FalhaProvedor> {
        Err(FalhaProvedor::Indisponivel(
            "cliente HTTPS ainda não implementado".into(),
        ))
    }
}

struct ProvedorGeminiRest {
    config: ConfigProvedor,
}

impl Provedor for ProvedorGeminiRest {
    fn nome(&self) -> &str {
        &self.config.nome
    }

    fn disponivel(&self) -> Result<(), FalhaProvedor> {
        if !self.config.habilitado {
            return Err(FalhaProvedor::Indisponivel("desabilitado na config".into()));
        }
        if self.config.chave.as_deref().unwrap_or("").is_empty() {
            return Err(FalhaProvedor::Indisponivel("sem chave de API".into()));
        }
        Err(FalhaProvedor::Indisponivel(
            "cliente HTTPS ainda não implementado (passo futuro) — Gemini fica de fora por ora"
                .into(),
        ))
    }

    fn responder(&self, _mensagem: &str, _contexto: &Contexto) -> Result<String, FalhaProvedor> {
        Err(FalhaProvedor::Indisponivel(
            "cliente HTTPS ainda não implementado".into(),
        ))
    }
}

#[cfg(test)]
mod testes {
    use super::*;
    use crate::config::ConfigProvedor;

    fn config_de(tipo: &str, habilitado: bool) -> ConfigProvedor {
        ConfigProvedor {
            nome: "teste".into(),
            tipo: tipo.into(),
            url_base: Some("http://127.0.0.1:11434".into()),
            modelo: Some("qwen2.5:1.5b".into()),
            comando: None,
            chave: None,
            timeout: Duration::from_secs(5),
            habilitado,
        }
    }

    #[test]
    fn construir_reconhece_tipos_conhecidos() {
        assert!(construir(&config_de("ollama", true)).is_some());
        assert!(construir(&config_de("claude_cli", true)).is_some());
        assert!(construir(&config_de("openai_compat", true)).is_some());
        assert!(construir(&config_de("gemini_rest", true)).is_some());
    }

    #[test]
    fn construir_ignora_tipo_desconhecido() {
        assert!(construir(&config_de("inventado", true)).is_none());
    }

    #[test]
    fn disponivel_falha_quando_desabilitado() {
        let provedor = construir(&config_de("ollama", false)).unwrap();
        assert!(provedor.disponivel().is_err());
    }

    #[test]
    fn groq_sem_chave_fica_indisponivel() {
        let provedor = construir(&config_de("openai_compat", true)).unwrap();
        // Sem chave -> indisponível (mensagem clara, nunca sucesso silencioso).
        assert!(matches!(
            provedor.disponivel(),
            Err(FalhaProvedor::Indisponivel(_))
        ));
    }
}
