//! Os provedores e o trait que os torna intercambiáveis.
//!
//! O coração do "agnosticismo": o roteador não sabe se está falando com Ollama, Claude,
//! Groq ou Gemini. Ele só conhece o trait `Provedor`. Trocar/adicionar provedor é
//! implementar o trait e citar o nome na `ordem_fallback`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::ConfigProvedor;
use crate::erro::FalhaProvedor;
use crate::http;
use crate::https;
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
        "resposta_fixa" => Some(Box::new(ProvedorRespostaFixa {
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

        // Tomamos posse dos três canos. Precisamos DRENAR stdout e stderr enquanto o
        // processo ainda roda: se a resposta do Claude passar do buffer do pipe do SO
        // (~64 KB), o processo BLOQUEIA escrevendo no stdout à espera de um leitor. Se só
        // fôssemos ler DEPOIS que ele terminasse, ele nunca terminaria (deadlock clássico)
        // e nós o mataríamos por "timeout" — perdendo uma resposta longa perfeitamente boa.
        // Por isso cada cano ganha sua própria thread, drenando em paralelo à espera.
        let stdin = filho
            .stdin
            .take()
            .ok_or_else(|| FalhaProvedor::Processo("sem stdin no processo".into()))?;
        let stdout = filho
            .stdout
            .take()
            .ok_or_else(|| FalhaProvedor::Processo("sem stdout no processo".into()))?;
        let stderr = filho
            .stderr
            .take()
            .ok_or_else(|| FalhaProvedor::Processo("sem stderr no processo".into()))?;

        // Thread de ESCRITA: manda o prompt e fecha o stdin (o `drop` do `stdin` ao fim
        // fecha o cano, sinalizando "fim da entrada"). Em thread para nunca travar caso o
        // buffer de stdin encha antes de o processo começar a ler.
        let prompt_bytes = prompt_texto.into_bytes();
        let escritor = std::thread::spawn(move || -> std::io::Result<()> {
            let mut stdin = stdin;
            stdin.write_all(&prompt_bytes)?;
            Ok(()) // o drop de `stdin` aqui fecha o cano
        });

        // Threads de LEITURA: cada uma lê seu cano até o EOF (que chega quando o processo
        // termina ou é morto). Devolvem os bytes lidos.
        let leitor_stdout = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut stdout = stdout;
            let mut buffer = Vec::new();
            stdout.read_to_end(&mut buffer)?;
            Ok(buffer)
        });
        let leitor_stderr = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut stderr = stderr;
            let mut buffer = Vec::new();
            stderr.read_to_end(&mut buffer)?;
            Ok(buffer)
        });

        // Espera com timeout próprio (a stdlib não tem wait com prazo): consultamos
        // `try_wait` num laço curto até o processo terminar ou estourar o tempo.
        let prazo = Instant::now() + self.config.timeout;
        let status = loop {
            match filho.try_wait() {
                Ok(Some(status)) => break status, // terminou
                Ok(None) => {
                    if Instant::now() >= prazo {
                        // Estourou: mata o processo (que é o `claude` direto — sem shell no
                        // meio) e o reapa para não virar zumbi. Ao morrer, seus canos fecham
                        // e as threads de leitura chegam ao EOF sozinhas. NÃO as juntamos
                        // aqui de propósito: se o processo tivesse deixado um neto segurando
                        // o cano, o `join` travaria o caminho da mensagem viva. Largamos as
                        // handles (as threads se desprendem e terminam quando o cano fechar).
                        let _ = filho.kill();
                        let _ = filho.wait();
                        return Err(FalhaProvedor::Processo("estourou o timeout".into()));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(FalhaProvedor::Processo(format!("erro ao aguardar: {e}"))),
            }
        };

        // Junta as threads e colhe o que cada cano produziu. `join` devolve o `Result` da
        // thread; um cano que falhou na leitura vira FalhaProvedor (nada de erro silencioso).
        let saida_stdout = leitor_stdout
            .join()
            .map_err(|_| FalhaProvedor::Processo("thread de stdout entrou em pânico".into()))?
            .map_err(|e| FalhaProvedor::Processo(format!("falha ao ler stdout: {e}")))?;
        let saida_stderr = leitor_stderr
            .join()
            .map_err(|_| FalhaProvedor::Processo("thread de stderr entrou em pânico".into()))?
            .map_err(|e| FalhaProvedor::Processo(format!("falha ao ler stderr: {e}")))?;
        // A escrita do stdin pode ter dado "broken pipe" se o processo morreu cedo; nesse
        // caso o motivo real está no status/stderr abaixo, então só reportamos o erro de
        // escrita quando ele NÃO for um cano quebrado (para não mascarar a causa raiz).
        let escrita = escritor
            .join()
            .map_err(|_| FalhaProvedor::Processo("thread de stdin entrou em pânico".into()))?;
        if let Err(e) = &escrita {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(FalhaProvedor::Processo(format!(
                    "falha ao escrever no stdin: {e}"
                )));
            }
        }

        if !status.success() {
            let erro = String::from_utf8_lossy(&saida_stderr);
            return Err(FalhaProvedor::Processo(format!(
                "código {:?}: {}",
                status.code(),
                erro.trim()
            )));
        }
        let texto = String::from_utf8_lossy(&saida_stdout).trim().to_string();
        if texto.is_empty() {
            return Err(FalhaProvedor::RespostaVazia);
        }
        Ok(texto)
    }
}

// --------------------------------------------------------------------------- //
// Provedores estilo OpenAI (`/chat/completions`) e Gemini (REST).
//
// O `openai_compat` cobre DOIS mundos com o mesmo código, escolhendo o transporte pelo
// esquema da URL configurada em `url_base`:
//   • `https://...`  → serviço EXTERNO na nuvem (Groq, OpenAI, OpenRouter...). Vai pelo
//     módulo `https` (curl/TLS) e EXIGE `chave` de API. Fica desabilitado até haver chave
//     (o Thiago cria a do Groq grátis; a do Gemini estava sem cota).
//   • `http://...`   → servidor LOCAL self-hosted que fala a mesma API (llama.cpp `--server`,
//     LM Studio, vLLM, LocalAI, ou o próprio Ollama em `/v1`). Vai pelo NOSSO cliente cru
//     `http` (TcpStream, sem TLS, zero deps) e NÃO precisa de chave — roda na máquina de
//     confiança. Isso dá ao robô um SEGUNDO provedor local (redundância perto do piso) sem
//     depender de nenhuma assinatura externa — exatamente o "agnosticismo" do projeto.
// Em ambos os casos: sem sucesso falso — qualquer erro vira FalhaProvedor.
// --------------------------------------------------------------------------- //

/// Verdadeiro se a URL usa o esquema HTTPS (serviço externo, exige TLS/curl e chave).
/// Comparação sem diferenciar maiúsculas, pois esquemas de URL são case-insensitive
/// (RFC 3986). `get(..8)` evita fatiar fora de fronteira de caractere (nunca faz panic).
/// `pub(crate)` para o doutor de config (`verificacao`) reusar a MESMA regra (DRY).
pub(crate) fn url_e_https(url: &str) -> bool {
    let inicio = url.trim_start();
    inicio
        .get(..8)
        .is_some_and(|prefixo| prefixo.eq_ignore_ascii_case("https://"))
}

/// Envia o POST OpenAI-compat escolhendo o transporte pelo esquema da URL: `https://`
/// pela pilha TLS (`https`/curl, externo); qualquer outra coisa pelo cliente cru `http`
/// (TcpStream, local). Mesma assinatura nos dois módulos, então só trocamos qual chamar.
fn enviar_openai_compat(
    url: &str,
    corpo_json: &str,
    cabecalhos_extra: &[(&str, &str)],
    tempo_limite: Duration,
) -> Result<crate::http::RespostaHttp, FalhaProvedor> {
    if url_e_https(url) {
        https::post_json(url, corpo_json, cabecalhos_extra, tempo_limite)
    } else {
        http::post_json(url, corpo_json, cabecalhos_extra, tempo_limite)
    }
}

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
        let url = self.config.url_base.as_deref().unwrap_or("");
        if url.is_empty() {
            return Err(FalhaProvedor::Indisponivel(
                "provedor sem 'url_base'".into(),
            ));
        }
        // Serviço externo (https) PRECISA de chave para autenticar; um servidor local
        // (http) roda na máquina de confiança e normalmente aceita sem chave nenhuma.
        if url_e_https(url) && self.config.chave.as_deref().unwrap_or("").is_empty() {
            return Err(FalhaProvedor::Indisponivel(
                "provedor OpenAI-compat externo (https) sem chave de API".into(),
            ));
        }
        Ok(())
    }

    fn responder(&self, mensagem: &str, contexto: &Contexto) -> Result<String, FalhaProvedor> {
        let url_base = self
            .config
            .url_base
            .as_deref()
            .ok_or_else(|| FalhaProvedor::Indisponivel("provedor sem 'url_base'".into()))?;
        let modelo = self
            .config
            .modelo
            .as_deref()
            .ok_or_else(|| FalhaProvedor::Indisponivel("provedor sem 'modelo'".into()))?;
        // A chave é OPCIONAL: obrigatória só para endpoint externo (https), onde a
        // pré-checagem `disponivel` já a exigiu. Para um servidor local (http) ela pode
        // faltar — aí não mandamos cabeçalho de autorização.
        let chave = self.config.chave.as_deref().filter(|c| !c.is_empty());

        let url = format!("{}/chat/completions", url_base.trim_end_matches('/'));

        // Corpo {"model":..., "messages":[{"role":...,"content":...}, ...]} com nosso JSON.
        let mensagens = prompt::montar_mensagens(mensagem, contexto)
            .into_iter()
            .map(|m| {
                Valor::Objeto(vec![
                    ("role".into(), Valor::Texto(m.papel)),
                    ("content".into(), Valor::Texto(m.conteudo)),
                ])
            })
            .collect::<Vec<_>>();
        let corpo = Valor::Objeto(vec![
            ("model".into(), Valor::Texto(modelo.to_string())),
            ("messages".into(), Valor::Lista(mensagens)),
        ])
        .para_texto();

        // Só anexa `Authorization: Bearer <chave>` quando há chave (endpoint externo).
        // O `autorizacao` precisa viver até o fim da chamada, por isso fica em variável.
        let autorizacao = chave.map(|c| format!("Bearer {c}"));
        let mut cabecalhos: Vec<(&str, &str)> = Vec::new();
        if let Some(valor) = autorizacao.as_deref() {
            cabecalhos.push(("Authorization", valor));
        }
        let resposta = enviar_openai_compat(&url, &corpo, &cabecalhos, self.config.timeout)?;
        if resposta.status != 200 {
            return Err(FalhaProvedor::Http {
                status: resposta.status,
                corpo: resposta.corpo,
            });
        }

        // Extrai choices[0].message.content.
        let raiz = json::parsear(&resposta.corpo)
            .map_err(|e| FalhaProvedor::RespostaInvalida(e.to_string()))?;
        let texto = raiz
            .obter("choices")
            .and_then(|c| c.indice(0))
            .and_then(|c| c.obter("message"))
            .and_then(|m| m.obter("content"))
            .and_then(Valor::como_texto)
            .ok_or_else(|| {
                FalhaProvedor::RespostaInvalida("sem choices[0].message.content".into())
            })?
            .trim()
            .to_string();
        if texto.is_empty() {
            return Err(FalhaProvedor::RespostaVazia);
        }
        Ok(texto)
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
        Ok(())
    }

    fn responder(&self, mensagem: &str, contexto: &Contexto) -> Result<String, FalhaProvedor> {
        let modelo = self
            .config
            .modelo
            .as_deref()
            .ok_or_else(|| FalhaProvedor::Indisponivel("gemini sem 'modelo'".into()))?;
        let chave = self
            .config
            .chave
            .as_deref()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| FalhaProvedor::Indisponivel("sem chave de API".into()))?;

        // A chave do Gemini vai na query string (padrão da API generativelanguage).
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{modelo}:generateContent?key={chave}"
        );

        // Corpo {"contents":[{"parts":[{"text": prompt}]}]}.
        let corpo = Valor::Objeto(vec![(
            "contents".into(),
            Valor::Lista(vec![Valor::Objeto(vec![(
                "parts".into(),
                Valor::Lista(vec![Valor::Objeto(vec![(
                    "text".into(),
                    Valor::Texto(prompt::montar_prompt(mensagem, contexto)),
                )])]),
            )])]),
        )])
        .para_texto();

        let resposta = https::post_json(&url, &corpo, &[], self.config.timeout)?;
        if resposta.status != 200 {
            return Err(FalhaProvedor::Http {
                status: resposta.status,
                corpo: resposta.corpo,
            });
        }

        // Extrai candidates[0].content.parts[0].text.
        let raiz = json::parsear(&resposta.corpo)
            .map_err(|e| FalhaProvedor::RespostaInvalida(e.to_string()))?;
        let texto = raiz
            .obter("candidates")
            .and_then(|c| c.indice(0))
            .and_then(|c| c.obter("content"))
            .and_then(|c| c.obter("parts"))
            .and_then(|p| p.indice(0))
            .and_then(|p| p.obter("text"))
            .and_then(Valor::como_texto)
            .ok_or_else(|| {
                FalhaProvedor::RespostaInvalida("sem candidates[0].content.parts[0].text".into())
            })?
            .trim()
            .to_string();
        if texto.is_empty() {
            return Err(FalhaProvedor::RespostaVazia);
        }
        Ok(texto)
    }
}

// --------------------------------------------------------------------------- //
// Resposta fixa — piso de ÚLTIMA instância que NUNCA falha.
//
// Não fala com rede, não sobe processo, não usa chave: só devolve um texto fixo da config
// (`mensagem_fixa`). Serve para uma garantia mais forte do que "o robô quase nunca fica
// mudo": se até o Ollama local cair, a cadeia ainda entrega uma mensagem de cortesia em vez
// de silêncio — o `rotear` deixa de poder devolver `TodosFalharam` quando este provedor
// fecha a ordem. É o provedor mais simples possível (bom exemplo didático do trait) e o
// candidato ideal a ÚLTIMO da `ordem_fallback`, abaixo do Ollama.
//
// Nunca dispara o Claude nem qualquer serviço — é 100% local e determinístico.
// --------------------------------------------------------------------------- //
struct ProvedorRespostaFixa {
    config: ConfigProvedor,
}

impl ProvedorRespostaFixa {
    /// A mensagem configurada, já sem espaços nas pontas — ou `None` se ausente/vazia.
    /// Centraliza a regra "vazio conta como não-configurado" usada por `disponivel`/`responder`.
    fn texto_configurado(&self) -> Option<&str> {
        self.config
            .mensagem_fixa
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }
}

impl Provedor for ProvedorRespostaFixa {
    fn nome(&self) -> &str {
        &self.config.nome
    }

    fn disponivel(&self) -> Result<(), FalhaProvedor> {
        if !self.config.habilitado {
            return Err(FalhaProvedor::Indisponivel("desabilitado na config".into()));
        }
        // Sem texto configurado não temos o que responder: fica indisponível (e a verificação
        // estática avisa antes de ir para produção, para o piso não quebrar em silêncio).
        if self.texto_configurado().is_none() {
            return Err(FalhaProvedor::Indisponivel(
                "resposta_fixa sem 'mensagem_fixa' (ou vazia)".into(),
            ));
        }
        Ok(())
    }

    fn responder(&self, _mensagem: &str, _contexto: &Contexto) -> Result<String, FalhaProvedor> {
        // Ignora a mensagem/contexto de propósito: é uma resposta fixa. Como `disponivel`
        // já garantiu que há texto, aqui não deveria faltar; ainda assim tratamos o caso
        // sem `unwrap` (nunca em produção), devolvendo falha tipada em vez de pânico.
        self.texto_configurado()
            .map(str::to_string)
            .ok_or_else(|| FalhaProvedor::Indisponivel("resposta_fixa sem 'mensagem_fixa'".into()))
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
            mensagem_fixa: None,
            timeout: Duration::from_secs(5),
            habilitado,
            retentativas: 0,
            retentativa_espera_ms: 250,
        }
    }

    #[test]
    fn construir_reconhece_tipos_conhecidos() {
        assert!(construir(&config_de("ollama", true)).is_some());
        assert!(construir(&config_de("claude_cli", true)).is_some());
        assert!(construir(&config_de("openai_compat", true)).is_some());
        assert!(construir(&config_de("gemini_rest", true)).is_some());
        assert!(construir(&config_de("resposta_fixa", true)).is_some());
    }

    /// Helper: config de um provedor `resposta_fixa` com a mensagem dada (ou nenhuma).
    fn config_resposta_fixa(mensagem: Option<&str>, habilitado: bool) -> ConfigProvedor {
        ConfigProvedor {
            nome: "piso_fixo".into(),
            tipo: "resposta_fixa".into(),
            url_base: None,
            modelo: None,
            comando: None,
            chave: None,
            mensagem_fixa: mensagem.map(str::to_string),
            timeout: Duration::from_secs(5),
            habilitado,
            retentativas: 0,
            retentativa_espera_ms: 250,
        }
    }

    #[test]
    fn resposta_fixa_devolve_o_texto_configurado() {
        let cfg = config_resposta_fixa(Some("Estou indisponível, tente já já."), true);
        let provedor = construir(&cfg).unwrap();
        assert!(provedor.disponivel().is_ok());
        let texto = provedor
            .responder("qualquer coisa", &Contexto::vazio())
            .unwrap();
        assert_eq!(texto, "Estou indisponível, tente já já.");
    }

    #[test]
    fn resposta_fixa_sem_mensagem_fica_indisponivel() {
        // Sem 'mensagem_fixa' (ou vazia/só espaços) não há o que responder → indisponível.
        for mensagem in [None, Some(""), Some("   ")] {
            let cfg = config_resposta_fixa(mensagem, true);
            let provedor = construir(&cfg).unwrap();
            assert!(matches!(
                provedor.disponivel(),
                Err(FalhaProvedor::Indisponivel(_))
            ));
        }
    }

    #[test]
    fn resposta_fixa_desabilitada_fica_indisponivel() {
        let cfg = config_resposta_fixa(Some("oi"), false);
        let provedor = construir(&cfg).unwrap();
        assert!(matches!(
            provedor.disponivel(),
            Err(FalhaProvedor::Indisponivel(_))
        ));
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
    fn openai_compat_externo_https_sem_chave_fica_indisponivel() {
        // URL https = serviço externo (Groq/OpenAI): sem chave -> indisponível (mensagem
        // clara, nunca sucesso silencioso). Este é o estado ESPERADO do Groq hoje.
        let mut config = config_de("openai_compat", true);
        config.url_base = Some("https://api.groq.com/openai/v1".into());
        config.chave = None;
        let provedor = construir(&config).unwrap();
        assert!(matches!(
            provedor.disponivel(),
            Err(FalhaProvedor::Indisponivel(_))
        ));
    }

    #[test]
    fn openai_compat_local_http_sem_chave_fica_disponivel() {
        // URL http = servidor LOCAL self-hosted (llama.cpp/LM Studio/vLLM): NÃO precisa de
        // chave, então fica DISPONÍVEL mesmo sem `chave` na config. É o novo caminho local.
        let mut config = config_de("openai_compat", true);
        config.url_base = Some("http://127.0.0.1:8080/v1".into());
        config.chave = None;
        let provedor = construir(&config).unwrap();
        assert!(provedor.disponivel().is_ok());
    }

    #[test]
    fn openai_compat_sem_url_base_fica_indisponivel() {
        // Sem url_base não há para onde mandar: indisponível já na pré-checagem.
        let mut config = config_de("openai_compat", true);
        config.url_base = None;
        let provedor = construir(&config).unwrap();
        assert!(matches!(
            provedor.disponivel(),
            Err(FalhaProvedor::Indisponivel(_))
        ));
    }

    #[test]
    fn url_e_https_reconhece_esquema_sem_diferenciar_maiuscula() {
        assert!(url_e_https("https://api.groq.com"));
        assert!(url_e_https("HTTPS://API.GROQ.COM"));
        assert!(url_e_https("  https://com-espaco-antes"));
        assert!(!url_e_https("http://127.0.0.1:8080"));
        assert!(!url_e_https("HTTP://local"));
        assert!(!url_e_https(""));
        assert!(!url_e_https("ftp://x"));
        // Não faz panic com multibyte curto no começo (get(..8) devolve None).
        assert!(!url_e_https("háçã"));
    }

    // ----------------------------------------------------------------------- //
    // Testes do provedor Claude CLI usando um PROGRAMA FALSO no lugar do `claude`.
    // Nunca disparam o Claude de verdade (licao-refresh-token-rotativo): o campo
    // `comando` aponta para um script de shell temporário que geramos aqui.
    // ----------------------------------------------------------------------- //
    #[cfg(unix)]
    fn escrever_script_temporario(corpo: &str) -> std::path::PathBuf {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        // Nome único por processo + contador (sem relógio/aleatório, para ser determinístico).
        static CONTADOR: AtomicU64 = AtomicU64::new(0);
        let sequencia = CONTADOR.fetch_add(1, Ordering::Relaxed);
        let caminho = std::env::temp_dir().join(format!(
            "roteador-fake-claude-{}-{}.sh",
            std::process::id(),
            sequencia
        ));
        let mut arquivo = std::fs::File::create(&caminho).expect("cria script temporário");
        arquivo
            .write_all(corpo.as_bytes())
            .expect("escreve script temporário");
        // Marca executável (0o755) — sem isso o `Command::spawn` falha com "permission denied".
        std::fs::set_permissions(&caminho, std::fs::Permissions::from_mode(0o755))
            .expect("torna script executável");
        caminho
    }

    #[cfg(unix)]
    fn config_claude_com(comando: std::path::PathBuf, timeout: Duration) -> ConfigProvedor {
        ConfigProvedor {
            nome: "claude-fake".into(),
            tipo: "claude_cli".into(),
            url_base: None,
            modelo: None,
            comando: Some(comando.to_string_lossy().into_owned()),
            chave: None,
            mensagem_fixa: None,
            timeout,
            habilitado: true,
            retentativas: 0,
            retentativa_espera_ms: 250,
        }
    }

    /// Regressão do deadlock de pipe: uma resposta MAIOR que o buffer do pipe do SO
    /// (~64 KB) deve voltar inteira. Antes de drenar stdout em thread, isto travava e caía
    /// num falso "timeout". O script ignora `--print`, ignora o stdin e cospe ~200 KB.
    #[cfg(unix)]
    #[test]
    fn claude_cli_le_resposta_maior_que_o_buffer_do_pipe() {
        let script = escrever_script_temporario(
            "#!/bin/sh\n# ignora $1 (--print); gera ~200 KB de 'x' no stdout e sai 0.\nhead -c 200000 /dev/zero | tr '\\0' x\n",
        );
        let config = config_claude_com(script.clone(), Duration::from_secs(10));
        let provedor = construir(&config).expect("constrói provedor claude falso");

        let resultado = provedor.responder("oi", &Contexto::default());
        let _ = std::fs::remove_file(&script);

        let texto = resultado.expect("resposta longa deve voltar inteira, sem deadlock/timeout");
        assert_eq!(texto.len(), 200_000, "todo o stdout deve ser lido");
        assert!(texto.chars().all(|c| c == 'x'));
    }

    /// O timeout continua matando um processo lento (sem deixá-lo órfão) e devolvendo falha.
    #[cfg(unix)]
    #[test]
    fn claude_cli_mata_processo_que_estoura_o_timeout() {
        // `exec` faz o shell VIRAR o sleep (sem neto), modelando o `claude` como filho
        // direto — assim o kill do timeout fecha o cano na hora, igual à produção.
        let script = escrever_script_temporario("#!/bin/sh\nexec sleep 30\n");
        let config = config_claude_com(script.clone(), Duration::from_millis(300));
        let provedor = construir(&config).expect("constrói provedor claude falso");

        let resultado = provedor.responder("oi", &Contexto::default());
        let _ = std::fs::remove_file(&script);

        match resultado {
            Err(FalhaProvedor::Processo(msg)) => assert!(
                msg.contains("timeout"),
                "esperava falha de timeout, veio: {msg}"
            ),
            outro => panic!("esperava FalhaProvedor::Processo(timeout), veio: {outro:?}"),
        }
    }
}
