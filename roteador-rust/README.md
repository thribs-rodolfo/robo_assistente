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
- **`https.rs`** — cliente HTTPS via `curl` (binário externo).
- **`provedor.rs`** — `claude --print` via `std::process`.

TLS/HTTPS **não** é reescrito à mão (criptografia séria, inviável sem crate). Em vez disso,
falamos HTTPS pelo `curl` — binário externo, exceção pragmática que o manifesto permite (o
mesmo princípio do `claude --print`). Assim os provedores remotos (Groq, Gemini) são **reais**
e mantemos **zero dependências de crates Rust**. Eles ficam desabilitados na config só porque
dependem de chave externa (o Thiago cria a do Groq; a do Gemini estava sem cota) — basta pôr a
chave e `"habilitado": true`. Em erro, sempre uma `FalhaProvedor` clara — **nunca** sucesso falso.

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
| `https.rs`      | HTTPS via `curl` (binário externo), espelha a interface do http |
| `prompt.rs`     | Monta prompt/mensagens a partir de (mensagem, contexto)         |
| `erro.rs`       | Erros tipados: `FalhaProvedor`, `ErroRoteador`                  |
| `config.rs`     | Lê a config JSON dos provedores (fora do repo)                  |
| `provedor.rs`   | Trait `Provedor` + Ollama, Claude CLI, Groq, Gemini             |
| `telemetria.rs` | Log de quem respondeu e por que caiu                            |
| `metricas.rs`   | Lê o log e agrega: de quem o robô realmente depende             |
| `alerta.rs`     | Decide (função pura) quando avisar o Thiago que caiu no piso    |
| `duracao.rs`    | Converte durações legíveis ("24h", "90m") <-> segundos          |
| `lib.rs`        | `rotear()` — a cadeia de fallback                               |
| `servidor_http.rs` | Servidor HTTP/1.1 cru (parse de requisição + resposta)       |
| `ponte.rs`      | Ponte Telegram: config dos bots, allowFrom, `sendMessage`, processar |
| `bin/ponte-telegram` | Servidor de webhooks que liga o Telegram ao `rotear()`     |
| `bin/metricas`  | Lê o log de telemetria e imprime as métricas (só leitura)       |
| `bin/alerta`    | Avisa o Thiago quando o robô cai no piso (Ollama) N vezes seguidas |

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

## Métricas (`bin/metricas`)

O `rotear()` só ANEXA linhas cruas ao log (`/var/log/roteador-provedores.log`).
O `bin/metricas` faz o caminho inverso: LÊ o log e responde **"de quem o robô
realmente depende?"**. É só leitura — nunca dispara provedor, seguro rodar à vontade.

```sh
./target/release/metricas                    # log padrão
./target/release/metricas /outro/caminho.log # outro arquivo
# == Métricas do roteador de provedores ==
# - groq: 0 ok, 9 falha, 1 pulo, 0 cfg | —
# - ollama_local: 3 ok, 10 falha, 0 pulo, 0 cfg | 1233ms média
# total de roteamentos: 3
# caiu no piso (Ollama): 3 de 3 (100.0%)
```

A linha-chave é a última: **quantas vezes caímos no piso (Ollama)**. Quanto maior o %,
mais o robô está rodando sem provedor bom — sinal pra investigar Claude/Groq/Gemini.

> Nota: linhas no formato ANTIGO do roteador Python (`... ,177 INFO [roteador]
> respondido por '...'`) são **ignoradas de propósito** (schema diferente) e contadas
> em "linhas ignoradas" — sem truncar em silêncio. A telemetria nova é toda em Rust.

## Alerta de dependência (`bin/alerta`)

As métricas a gente lê quando quer. O `bin/alerta` é o **aviso automático**: roda no cron,
LÊ o log (nunca dispara provedor → não toca o Claude) e, quando o robô cai no piso (Ollama)
**N vezes SEGUIDAS** — a cadeia de provedores bons falhando em série —, manda uma mensagem
pro Thiago via `/root/notificar-thiago.sh`.

São **dois alarmes ortogonais** sobre o mesmo log, cada um com seu anti-spam:

1. **Sequência** — quedas no piso **SEGUIDAS** (cadeia de cima falhando em série AGORA).
   Função pura `alerta::decidir`. Anti-spam por arquivo de estado: avisa **uma vez por
   rajada** e de novo só quando piora um degrau inteiro (mais `limite` quedas). Quando um
   provedor bom responde, a sequência zera e o estado é limpo.
2. **Percentual** — **fração alta** de quedas no piso na janela, *mesmo sem quedas em série*
   (cadeia falhando de forma intermitente mas pesada — ex.: 8 de 10 roteamentos no piso, sem
   nunca acumular 5 seguidas). Função pura `alerta::decidir_por_percentual`. Pega o que o alarme
   de sequência deixa passar. Anti-spam com **histerese**: avisa ao cruzar o limiar, fica quieto
   enquanto continua alto e só re-arma quando a fração cai com folga (`limiar − 15` pontos),
   evitando ligar/desligar na fronteira. Exige um **mínimo de amostras** para não alertar com
   pouca evidência (ex.: "1 de 1 = 100%").

Ambos só LÊEM o log — nunca disparam provedor → não tocam o Claude.

```sh
./target/release/alerta --simular              # decide e imprime, NÃO manda Telegram nem grava estado
./target/release/alerta --limite 5 --limiar-percentual 70 --janela 24h
# [alerta] seq_no_piso=3 limite=5 ja_alertado=0 -> notificar=false novo_estado=0
# [alerta] pct_no_piso=100% (3/3) limiar=70% min_amostras=8 ja_em_alta=false -> notificar=false novo_estado=false
```

| Opção                  | Default                                       | O que faz                                  |
|------------------------|-----------------------------------------------|--------------------------------------------|
| `--limite N`           | 5                                             | quedas **seguidas** no piso para alertar   |
| `--limiar-percentual N`| 70                                            | **%** no piso na janela para alertar       |
| `--minimo-amostras N`  | 8                                             | roteamentos mínimos p/ o alarme % valer    |
| `--janela <dur>`       | (tudo)                                        | só considera as últimas `<dur>` (24h, 90m…) |
| `--estado <p>`         | `/var/log/roteador-alerta-piso.estado`        | estado anti-spam do alarme de sequência    |
| `--estado-percentual <p>` | `/var/log/roteador-alerta-percentual.estado` | estado anti-spam do alarme percentual    |
| `--notificador <p>`    | `/root/notificar-thiago.sh`                   | script que manda a mensagem                |
| `--simular`            | —                                             | dry-run: não notifica nem grava            |

No cron (`/root/alerta-piso-roteador.sh`, a cada 30min): `alerta --limite 5 --janela 24h`
(os defaults de percentual entram automaticamente). Os limites são conservadores de propósito:
só incomodam o Thiago quando a degradação é clara — em série **ou** em fração alta.

## Ponte Telegram (`bin/ponte-telegram`)

Substitui o `servidor.py`. É o **lado servidor** da ponte: o Telegram entrega webhooks
(via nginx, que termina o HTTPS) em `http://127.0.0.1:18800`. Rotas:

- `POST /ponte-telegram/<nome_bot>` — recebe um update do Telegram.
- `GET  /ponte-telegram/saude` — healthcheck (responde `ok`).

Fluxo de um POST (espelha o `servidor.py`, agora tipado e sem exceções):

```
1. acha o bot pelo nome do caminho                          -> 404 se não existir
2. valida o secret do webhook (cabeçalho                    -> 403 se não bater
   X-Telegram-Bot-Api-Secret-Token)
3. responde 200 IMEDIATAMENTE ao Telegram                   (não segura a conexão)
4. extrai a mensagem; checa allowFrom (lista branca)        -> ignora quem não está
5. rotear(texto, contexto, config)                          -> texto + qual provedor
6. enviar_mensagem(token, chat, texto)  (sendMessage HTTPS via curl)
```

Config dos bots (com **TOKENS**) mora **fora do repo**, em `/root/.secrets/ponte-telegram.json`:

```json
{
  "bots": {
    "ronaldo": {
      "token": "123:ABC",
      "secret": "segredo-do-webhook",
      "allow_from": [8632113465],
      "sistema": "Você é o Ronaldo, assistente conciso em pt-BR."
    }
  }
}
```

`allow_from` pode ser inline (lista de IDs) **ou** `allow_from_arquivo` (aponta para um JSON
com a chave `allowFrom`). Lista vazia = ninguém (seguro por padrão). O endereço de escuta pode
ser trocado por `PONTE_ENDERECO` (útil para testar numa porta descartável).

```sh
cargo build --release
PONTE_ENDERECO=127.0.0.1:18877 ./target/release/ponte-telegram &
curl http://127.0.0.1:18877/ponte-telegram/saude          # -> ok
```

## Testes

```sh
cargo test                                              # testes unitários (puros, rápidos)
cargo test --test integracao_ollama -- --ignored        # teste AO VIVO contra o Ollama local
cargo clippy --all-targets -- -D warnings               # lint estrito, zero warning
cargo fmt --check                                        # formatação
```

> O teste ao vivo usa **só** o Ollama (a ordem não inclui o Claude), de propósito: nunca
> disparamos o Claude "só pra testar" (evita risco no refresh do token OAuth).

## Estado (passos 1 e 2 do plano Rust — concluídos)

- [x] Trait `Provedor` + cadeia de fallback
- [x] `ProvedorOllama` (HTTP cru) — **provado ao vivo**: respondeu "Paris."
- [x] `ProvedorClaudeCli` (`claude --print`, sem tocar no refresh)
- [x] `ProvedorOpenAiCompat` (Groq) e `ProvedorGeminiRest` via HTTPS (curl) — código
      completo; transporte **provado ao vivo** (HTTP 400 estruturado do Gemini com chave
      inválida). Faltam só as chaves para habilitar.
- [x] **Ponte Telegram em Rust** (`bin/ponte-telegram`): servidor HTTP/1.1 cru, validação de
      secret, allowFrom (inline ou arquivo), chama o `rotear()` e responde via `sendMessage`.
      **Provada ao vivo** sobre TCP real: healthcheck 200, rota/bot inexistente 404, secret
      errado 403 (rejeitado **antes** de chegar ao roteador — Claude nunca disparado).
- [ ] Próximo: apontar o webhook do Ronaldo para a ponte Rust e testar fim-a-fim com fallback
      (o `servidor.py` segue rodando até a troca — sem buraco no ar).
