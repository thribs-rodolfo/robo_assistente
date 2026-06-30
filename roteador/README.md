# Roteador de provedores (cérebro agnóstico)

A ponte/agente deixa de chamar um provedor de LLM fixo (ex.: `claude --print`) e passa a chamar
este **roteador**, que tenta os provedores em **ordem de fallback**. Se um falha (sem chave, 401,
429, timeout, erro), cai pro próximo. Assim o robô **não depende de uma assinatura só**.

## Interface comum

Cada provedor implementa:

```python
responder(mensagem, contexto) -> texto
```

- `mensagem`: o texto do usuário (str).
- `contexto`: dict opcional com `sistema` (instrução de sistema) e `historico`
  (lista de turnos `{"autor": "usuario"|"assistente", "texto": "..."}`).

Ponto de entrada do roteador:

```python
from roteador import rotear
texto, provedor = rotear("qual a capital do Brasil?")
```

`rotear` devolve `(texto, nome_do_provedor)` ou levanta `RuntimeError` só se **todos** falharem
(não deve acontecer com o Ollama local na cauda).

## Provedores suportados

| Tipo (config)    | Classe                 | Como fala                                   |
|------------------|------------------------|---------------------------------------------|
| `claude_cli`     | `ProvedorClaudeCLI`    | `claude --print` (token OAuth da máquina)   |
| `openai_compat`  | `ProvedorOpenAICompat` | API compatível com OpenAI (ex.: Groq)       |
| `gemini_rest`    | `ProvedorGeminiREST`   | REST `generativelanguage` do Google         |
| `ollama`         | `ProvedorOllama`       | API local do Ollama (`/api/generate`)       |

## Cadeia de fallback

A ordem fica em `ordem_fallback` na config. Regras:

- Tenta cada provedor na ordem; o primeiro que responder vence.
- **Pré-checagem barata** (`disponivel()`): provedor desabilitado ou sem chave é pulado **sem
  gastar rede**.
- O **Ollama local fica SEMPRE por último**: é o piso de emergência (custo zero, sempre vivo),
  pra a ponte nunca ficar muda.
- O provider Claude **nunca mexe no refresh do token** — se o `claude --print` falhar, apenas
  cai pro próximo. (Quem rotaciona o refresh é só o cron de produção.)

## Configuração

Config e chaves ficam **fora do repositório**, em `/root/.secrets/roteador-provedores.json`
(`chmod 600`). Veja `roteador-provedores.exemplo.json` como modelo. **Nunca** versione chaves reais.

## Telemetria

Cada resposta registra qual provedor atendeu em `/var/log/roteador-provedores.log` — serve pra
medir a dependência real de cada provedor.

## Teste rápido

```bash
python3 roteador/roteador.py "Diga apenas: roteador ok."
```

## Estado (passo a passo)

- [x] **Passo 1** — esqueleto + provider Ollama local (provado ao vivo) + provider Claude (`claude --print`).
- [ ] Passo 2 — ativar Groq e Gemini quando houver chave válida.
- [ ] Passo 3 — ligar a ponte-telegram no roteador.
- [ ] Passo 4 — teste fim-a-fim com fallback.
- [ ] Passo 5 — pedido de incorporação + README de arquitetura (canais × provedores).
