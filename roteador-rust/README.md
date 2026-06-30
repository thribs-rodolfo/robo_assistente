# roteador-rust

Roteador de provedores (cérebro agnóstico) **em Rust**, com **zero dependências** — só a
biblioteca padrão. A ponte/agente não chama mais um provedor direto: chama o `rotear()`, que
tenta os provedores em **ordem de fallback**. Se um falha (sem chave, 401, 429, timeout, erro),
cai para o próximo. O **Ollama local fica sempre por último**: piso de emergência, custo zero,
nunca deixa o robô mudo.

> É a reescrita em Rust do antigo `roteador.py` (que segue rodando até o Rust assumir). Padrão de
> código: ver `WORKSPACE_RULES` — Rust idiomático, comentado em pt-BR, educacional.

## Por que zero dependências

O manifesto pede o **mínimo de dependências** e código que dá pra **aprender por baixo**. Então
escrevemos à mão, só com a stdlib:

- **`json.rs`** — parser de descida recursiva + codificador de JSON.
- **`http.rs`** — cliente HTTP/1.1 cru sobre `TcpStream` (HTTP simples, para o Ollama local).
- **`provedor.rs`** — `claude --print` via `std::process`.

TLS/HTTPS **não** é reescrito à mão (inviável sem crate). Por isso os provedores remotos
(Groq, Gemini) ficam como *slots* honestos: declarados, mas devolvem uma falha clara até
decidirmos a abordagem de HTTPS num passo futuro — **nunca** um sucesso falso.

## Arquitetura

```
rotear(mensagem, contexto, config)
  └─ para cada nome em config.ordem_fallback:
       construir(tipo) -> Box<dyn Provedor>
       provedor.disponivel()?   // pré-checagem barata (habilitado? tem chave?)
       provedor.responder()     // sucesso -> devolve (texto, provedor)
                                // falha   -> telemetria + cai pro próximo
```

O **agnosticismo** mora no trait `Provedor`. Adicionar/trocar provedor = implementar o trait
e citar o nome na `ordem_fallback`.

| Módulo          | Papel                                                            |
|-----------------|-----------------------------------------------------------------|
| `json.rs`       | JSON próprio (parse + encode), com testes                       |
| `http.rs`       | HTTP/1.1 cru sobre TcpStream (sem TLS), com timeouts            |
| `prompt.rs`     | Monta prompt/mensagens a partir de (mensagem, contexto)         |
| `erro.rs`       | Erros tipados: `FalhaProvedor`, `ErroRoteador`                  |
| `config.rs`     | Lê a config JSON dos provedores (fora do repo)                  |
| `provedor.rs`   | Trait `Provedor` + Ollama, Claude CLI, slots Groq/Gemini        |
| `telemetria.rs` | Log de quem respondeu e por que caiu                            |
| `lib.rs`        | `rotear()` — a cadeia de fallback                               |

## Config

Mora **fora do repositório**, com as chaves reais, em
`/root/.secrets/roteador-provedores.json`. Exemplo **sem chaves**:

```json
{
  "ordem_fallback": ["groq", "gemini", "claude", "ollama_local"],
  "provedores": {
    "claude":       {"tipo": "claude_cli", "comando": "claude", "timeout_segundos": 120, "habilitado": true},
    "groq":         {"tipo": "openai_compat", "url_base": "https://api.groq.com/openai/v1",
                     "modelo": "llama-3.1-8b-instant", "chave": "SUA_CHAVE", "timeout_segundos": 30, "habilitado": false},
    "gemini":       {"tipo": "gemini_rest", "modelo": "gemini-1.5-flash",
                     "chave": "SUA_CHAVE", "timeout_segundos": 30, "habilitado": false},
    "ollama_local": {"tipo": "ollama", "url_base": "http://127.0.0.1:11434",
                     "modelo": "qwen2.5:1.5b", "timeout_segundos": 180, "habilitado": true}
  }
}
```

## Uso

```sh
cargo build --release
./target/release/roteador "qual a capital da França?"
# [provedor: ollama_local]
# Paris.
```

## Testes

```sh
cargo test                                              # 23 testes unitários (puros, rápidos)
cargo test --test integracao_ollama -- --ignored        # teste AO VIVO contra o Ollama local
cargo clippy --all-targets -- -D warnings               # lint estrito, zero warning
cargo fmt --check                                        # formatação
```

> O teste ao vivo usa **só** o Ollama (a ordem não inclui o Claude), de propósito: nunca
> disparamos o Claude "só pra testar" (evita risco no refresh do token OAuth).

## Estado (passo 1 do plano Rust — concluído)

- [x] Trait `Provedor` + cadeia de fallback
- [x] `ProvedorOllama` (HTTP cru) — **provado ao vivo**: respondeu "Paris."
- [x] `ProvedorClaudeCli` (`claude --print`, sem tocar no refresh)
- [x] Slots Groq/Gemini (honestos, bloqueados em HTTPS/chave)
- [ ] Próximo: ponte-telegram em Rust + HTTPS para Groq/Gemini
