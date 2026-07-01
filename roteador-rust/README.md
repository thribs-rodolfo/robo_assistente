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
| `http.rs`       | HTTP/1.1 cru sobre TcpStream (sem TLS), com timeouts + decodifica `chunked` |
| `https.rs`      | HTTPS via `curl` (binário externo), espelha a interface do http |
| `prompt.rs`     | Monta prompt/mensagens/conversa a partir de (mensagem, contexto) |
| `historico.rs`  | Memória curta de conversa por chat (últimos turnos em disco) — opt-in |
| `erro.rs`       | Erros tipados: `FalhaProvedor`, `ErroRoteador`                  |
| `config.rs`     | Lê a config JSON dos provedores (fora do repo)                  |
| `provedor.rs`   | Trait `Provedor` + Ollama, Claude CLI, Groq, Gemini, resposta fixa |
| `telemetria.rs` | Log de quem respondeu e por que caiu                            |
| `metricas.rs`   | Lê o log e agrega: de quem o robô realmente depende             |
| `alerta.rs`     | Decide (função pura) quando avisar o Thiago que caiu no piso    |
| `duracao.rs`    | Converte durações legíveis ("24h", "90m") <-> segundos          |
| `verificacao.rs`| Doutor ESTÁTICO da config (piso, ordem, chaves) — funções puras |
| `diagnostico.rs`| Doutor VIVO do piso: sonda o Ollama (`GET /api/tags`) — nunca toca Claude |
| `orcamento.rs`  | Orçamento de tempo TOTAL da cadeia: pula provedores de cima após o limite (piso nunca) — opt-in |
| `lib.rs`        | `rotear()` — a cadeia de fallback                               |
| `servidor_http.rs` | Servidor HTTP/1.1 cru (parse de requisição + resposta)       |
| `ponte.rs`      | Ponte Telegram: config dos bots, allowFrom, `sendMessage`, processar |
| `bin/ponte-telegram` | Servidor de webhooks que liga o Telegram ao `rotear()`     |
| `bin/metricas`  | Lê o log de telemetria e imprime as métricas (só leitura)       |
| `bin/alerta`    | Avisa o Thiago quando o robô cai no piso (Ollama) N vezes seguidas |
| `bin/verificar-config` | Confere a config antes do deploy (só leitura, nunca dispara provedor) |
| `bin/diagnostico` | Sonda se o piso (Ollama) está vivo com o modelo certo — nunca toca Claude |
| `bin/disjuntor` | Inspeciona o estado do disjuntor: quais circuitos estão abertos (só leitura) |

## Config

Mora **fora do repositório**, com as chaves reais, em
`/root/.secrets/roteador-provedores.json`. Exemplo **sem chaves**:

```json
{
  "ordem_fallback": ["groq", "gemini", "claude", "ollama_local"],
  "provedores": {
    "claude":       {"tipo": "claude_cli", "comando": "claude", "timeout_segundos": 120, "habilitado": true},
    "groq":         {"tipo": "openai_compat", "url_base": "https://api.groq.com/openai/v1",
                     "modelo": "llama-3.1-8b-instant", "chave": "SUA_CHAVE", "timeout_segundos": 30, "habilitado": false,
                     "retentativas": 2, "retentativa_espera_ms": 250},
    "gemini":       {"tipo": "gemini_rest", "modelo": "gemini-1.5-flash",
                     "chave": "SUA_CHAVE", "timeout_segundos": 30, "habilitado": false},
    "ollama_local": {"tipo": "ollama", "url_base": "http://127.0.0.1:11434",
                     "modelo": "qwen2.5:1.5b", "timeout_segundos": 180, "habilitado": true}
  },
  "disjuntor": {"habilitado": false, "sombra": false, "limiar_falhas": 3,
                "cooldown_segundos": 60, "cooldown_maximo_segundos": 1800}
}
```

O bloco `disjuntor` é **opcional** e vem **desligado por padrão** (ver abaixo).

### Provedor Ollama: persona pelo campo `system` nativo

O piso (Ollama, `qwen2.5:1.5b`) é o modelo **mais fraco** da cadeia, e toda a garantia "o
robô nunca fica mudo" repousa nele. A `/api/generate` do Ollama tem um campo **`system`
dedicado** para a persona/instrução, separado do `prompt`. O provedor passa a persona por
esse campo (via `corpo_ollama`, função pura testável) e manda no `prompt` **só a conversa**
(`prompt::montar_conversa` — histórico + mensagem), em vez de amassar tudo num texto único.
Modelos pequenos **aderem melhor** à instrução quando ela vem no campo certo, e a persona
**não se duplica** dentro do prompt. Sem `sistema` na config (ou vazio), o campo `system`
nem é enviado — corpo idêntico ao formato antigo (retrocompatível). Provedores que só aceitam
um bloco de texto (Gemini) seguem usando `prompt::montar_prompt` = sistema + conversa.

### Provedor `openai_compat`: nuvem (https) OU servidor local (http)

O tipo `openai_compat` (endpoint estilo OpenAI, `POST /chat/completions`) cobre **dois
mundos** com o mesmo código, escolhendo o transporte pelo **esquema da `url_base`**:

| `url_base` | Transporte | Chave | Exemplo |
|---|---|---|---|
| `https://…` | `https` (curl/TLS) | **obrigatória** | Groq, OpenAI, OpenRouter |
| `http://…`  | `http` (TcpStream cru, zero deps) | **opcional** | llama.cpp `--server`, LM Studio, vLLM, LocalAI, Ollama em `/v1` |

Um servidor OpenAI-compatível **rodando na própria máquina** (http) é confiável e
normalmente aceita requisição **sem chave** — então basta apontar a `url_base` para ele:

```json
"modelo_local": {"tipo": "openai_compat", "url_base": "http://127.0.0.1:8080/v1",
                 "modelo": "meu-modelo", "timeout_segundos": 60, "habilitado": true}
```

Isso dá ao robô um **segundo provedor local** além do Ollama nativo (mais redundância perto
do piso), **sem depender de nenhuma assinatura externa** — exatamente o "agnosticismo" do
projeto. Quando a `url_base` é `https://`, o comportamento é o de sempre: TLS via curl e
chave de API obrigatória (a pré-checagem `disponivel()` pula o provedor se faltar). A
verificação estática (`bin/verificar-config`) entende essa distinção: `openai_compat` local
(http) habilitado **sem** chave é uso normal (sem aviso); só `https` sem chave é sinalizado.

Campo opcional `"telemetria_log"`: caminho do log de telemetria. Ausente → o log de
produção (`/var/log/roteador-provedores.log`). Existe para NÃO haver um caminho global
escondido: os testes apontam para um arquivo temporário e o roteamento fica **hermético**
— antes, rodar `cargo test` gravava linhas de teste (portas mortas) no log de produção e
**contaminava as métricas** do `bin/metricas` (a medida de "% no piso", que é o objetivo
do projeto). Produção não precisa declará-lo.

## Cliente HTTP: leitura de resposta `chunked`

O `http.rs` fala HTTP/1.1 cru (para o Ollama local, sem TLS). Um servidor HTTP/1.1 pode
enviar o corpo em **`Transfer-Encoding: chunked`** — a resposta chega em pedaços, cada um
precedido pelo seu tamanho em hexadecimal, terminando num pedaço de tamanho 0. Ler o corpo
"cru" nesse caso deixaria a **moldura dos chunks** (linhas de tamanho + terminadores)
misturada aos dados: o JSON do Ollama sairia sujo e o parser falharia — o **piso apareceria
"quebrado" mesmo respondendo**, jogando o robô para a resposta de cortesia à toa.

Por isso o `interpretar_resposta` detecta o cabeçalho (sem diferenciar maiúsculas, aceitando
codificação composta como `gzip, chunked`) e **decodifica os chunks**, devolvendo só os dados
remontados. Chunk truncado/malformado vira `FalhaProvedor::Rede` — nunca um corpo pela metade
em silêncio. Hoje o Ollama responde com `Content-Length` (caminho idêntico ao de antes); esta
é uma rede de segurança para versões/servidores que usem `chunked`.

## Piso de última instância: provedor `resposta_fixa`

A garantia central do projeto é **"o robô nunca fica mudo"**, e ela repousa no piso (o
Ollama local, último da ordem). Mas o Ollama **pode cair** (processo morto, máquina sem
recurso). Nesse caso a cadeia inteira falha e o `rotear` devolve `TodosFalharam` — o usuário
fica sem resposta.

O provedor **`resposta_fixa`** fecha esse buraco. É o provedor mais simples possível: não
fala com rede, não sobe processo, não usa chave — só devolve um texto fixo da config
(`mensagem_fixa`). Posto como **último** da `ordem_fallback` (abaixo do Ollama), ele garante
que a cadeia **nunca** devolve `TodosFalharam`: se até o Ollama cair, o robô ainda entrega
uma mensagem de cortesia em vez de silêncio.

```json
"ordem_fallback": ["claude", "ollama_local", "cortesia"],
"provedores": {
  "cortesia": {"tipo": "resposta_fixa",
               "mensagem_fixa": "Estou com uma instabilidade técnica agora. Tente de novo em instantes."}
}
```

- **Nunca dispara o Claude** nem qualquer serviço — é 100% local e determinístico.
- Sem `mensagem_fixa` (ou vazia) ele fica **indisponível** e é pulado; o `bin/verificar-config`
  marca isso como **erro** antes do deploy (piso não pode quebrar em silêncio).
- Os dois "doutores" reconhecem o novo piso: `verificar-config` aceita `resposta_fixa` como
  piso legítimo (local, nunca mudo) e o `bin/diagnostico` o reporta como **sempre vivo**
  (não há rede a sondar), em vez de "não verificado".

> Nota: a ponte Telegram já manda um aviso de cortesia embutido quando o `rotear` devolve
> `TodosFalharam`. O `resposta_fixa` leva essa garantia para **dentro** do roteador, de modo
> que **qualquer** chamador de `rotear()` (a ponte, o `bin/roteador`, outra ferramenta)
> herde o piso — defesa em profundidade, configurável e testável.

## Disjuntor / circuit breaker (`disjuntor`)

**Problema que resolve:** o Claude é frágil (o token OAuth cai). Com o roteamento linear
puro, enquanto o Claude está fora, TODA mensagem tenta o Claude primeiro e paga o timeout
inteiro antes de cair pro Ollama — lentidão à toa, mensagem após mensagem.

**Como funciona:** o disjuntor lembra as falhas RECENTES de cada provedor. Depois de
`limiar_falhas` falhas seguidas, "abre o circuito" daquele provedor por `cooldown_segundos`
— nesse intervalo o roteador o **pula** sem gastar rede/processo. Passado o cooldown, deixa
passar UMA tentativa ("meio-aberto"): sucesso fecha o circuito, nova falha reabre.

- **Backoff exponencial no cooldown.** A primeira abertura espera `cooldown_segundos`; cada
  reabertura seguida (meio-aberto que falha de novo) **dobra** o intervalo — `60 → 120 → 240
  → …` — até o teto `cooldown_maximo_segundos`. Assim um provedor que volta rápido sofre
  pouca espera, mas um que fica fora por horas (token do Claude caído) para de ser sondado a
  cada minuto: menos latência gasta com provedor morto, sem nunca esquecê-lo (o teto garante
  que ele volta a ser testado). Um sucesso zera o contador e a próxima rajada recomeça no base.

- **O piso (Ollama local, último da ordem) NUNCA é pulado** → o robô nunca fica mudo.
- **Só falhas de INDISPONIBILIDADE abrem o circuito.** Nem toda falha significa "provedor
  fora". O roteador classifica (ver `FalhaProvedor::indica_provedor_indisponivel`):
  - **Conta** (provedor fora/rejeitando, vale pular): rede/timeout, processo (`claude`
    caído), HTTP `401`/`403` (auth), `408`, `429` (rate limit), `5xx`.
  - **Não conta** (o provedor está de pé, o problema é DAQUELA mensagem): HTTP
    `400`/`404`/`413`/`422`, resposta vazia, resposta em formato inesperado.
  Assim uma mensagem malformada (um 400) não "queima" um provedor são — abrir o circuito
  dele jogaria as próximas mensagens boas no piso à toa, o **oposto** do objetivo do
  projeto (depender MENOS do piso). Uma falha que não conta deixa o contador de falhas
  seguidas **intacto** (nem soma, nem zera) e sai como `[roteamento] <nome>: falha da
  mensagem — não conta pro disjuntor`.
- **Estado operacional e efêmero** em `/var/log/roteador-disjuntor.estado` (fora do repo);
  ausente/corrompido → tudo tratado como fechado (= comportamento antigo). Degrada com graça.
- **Gravação ATÔMICA do estado** (temporário + `rename`, ver módulo `arquivo`): a ponte atende
  cada mensagem numa thread própria, então dois roteamentos podem gravar este arquivo "ao mesmo
  tempo". A escrita ingênua (truncate + write) deixa o arquivo vazio/pela metade por um instante,
  e um leitor concorrente (outra thread, o `bin/disjuntor`, um cron) o veria "corrompido → tudo
  fechado", **esquecendo os circuitos abertos justo sob carga** — o oposto do objetivo. Escrever
  num temporário e renomear por cima (atômico no POSIX, mesmo FS) garante que todo leitor vê o
  estado ANTIGO inteiro ou o NOVO inteiro, nunca um meio-termo.
- **Desligado por padrão:** sem o bloco `disjuntor` (ou com `"habilitado": false`), o
  roteador nem lê o arquivo e o comportamento é idêntico ao de antes (risco zero).
- Telemetria: um pulo pelo disjuntor sai como `[disjuntor] <nome>: disjuntor aberto (N
  falhas seguidas) — pulando`.

Campos (todos opcionais, com padrão): `habilitado` (false), `sombra` (false, ver abaixo),
`limiar_falhas` (3), `cooldown_segundos` (60, cooldown BASE), `cooldown_maximo_segundos`
(1800, teto do backoff), `caminho_estado` (`/var/log/roteador-disjuntor.estado`).

### Modo sombra (`sombra: true`) — a rampa de confiança

Ligar o disjuntor em produção **dá medo**: e se ele pular um provedor que na verdade ia
responder? O modo **sombra** (dry-run) responde essa pergunta com **risco zero**. Com
`{"habilitado": true, "sombra": true}`, o disjuntor **observa e aprende** (conta falhas,
abre/fecha circuitos, grava estado) mas **NÃO pula ninguém** — o roteamento fica **idêntico
ao de hoje**. Em vez de pular, ele só **registra na telemetria o que FARIA** se estivesse
ativo, comparando a previsão com o que **realmente** aconteceu:

- **Previsão certa** (pularia um provedor que de fato falhou):
  `[disjuntor-sombra] pularia '<nome>' (circuito aberto, N falhas seguidas) e teria
  economizado ~<ms>ms — ele falhou como previsto`. Esse `<ms>` é a latência REAL que o
  provedor morto acabou de gastar — ou seja, exatamente o que o disjuntor ligado pouparia
  naquela mensagem. É a economia virando número, medida no tráfego real.
- **Falso positivo** (pularia um provedor que RESPONDEU):
  `[disjuntor-sombra] PULARIA '<nome>' ... mas ele RESPONDEU em <ms>ms — FALSO POSITIVO`.
  É o sinal de ouro: mostra que ligar o disjuntor agora custaria uma resposta boa → suba o
  `limiar_falhas`/`cooldown` antes de ativar.

Fluxo sugerido: rode um tempo com `sombra: true`, leia essas linhas (só `grep disjuntor-sombra`
no log de telemetria), confira que só há previsões certas (sem falsos positivos) e a economia
compensa — **então** troque para `sombra: false` e o disjuntor passa a pular de verdade. Sem
efeito quando `habilitado: false`.

### Inspecionar o disjuntor (`bin/disjuntor`)

O estado do disjuntor vive num arquivo JSON opaco. Sem ver "quais circuitos estão abertos
agora e por quanto tempo", **ligar o disjuntor em produção dá medo** — por isso ele ainda
está desligado. O `bin/disjuntor` é o olho: lê a config (para achar o `caminho_estado`) e o
arquivo de estado e mostra, por provedor, se está **aberto** (sendo pulado) com o restante do
cooldown, ou fechado com quantas falhas já acumulou. É **só leitura**: nunca constrói
provedor, nunca abre socket, **nunca dispara o Claude**.

```
$ disjuntor                      # config padrão em /root/.secrets/...
== Disjuntor do roteador ==
estado: LIGADO (abre com 3 falhas seguidas, cooldown base 1min)
  🔴 claude: ABERTO (pulado) — reabre em 5min · 4 falha(s) seguida(s)
  🟢 gemini: fechado (deixa passar) · 2 falha(s) acumulada(s)
```

Código de saída (para monitoramento/cron): `0` = nenhum circuito aberto, `1` = ao menos um
provedor sendo pulado agora, `2` = erro de uso / config ilegível. Quando o disjuntor está
**desligado** na config, o relatório avisa que as linhas são só resíduo (não afetam o
roteamento).

## Retentativa em falhas transitórias (`retentativas`)

**Problema que resolve:** um blip PASSAGEIRO num provedor bom (a rede piscou, veio um
`503`/`429` momentâneo) fazia o roteamento cair direto para o próximo — no fim, para o piso
Ollama, lento e fraco. Mas uma segunda tentativa logo em seguida costuma passar. Retentar no
provedor bom antes de desistir dele = **depender menos do piso** (o objetivo do projeto).

**Como funciona:** ao falhar, o roteador pergunta a `retentativa::planejar` se vale retentar.
Só retenta falhas **transitórias**: `Rede` (socket piscou/timeout) e HTTP `408`/`429`/`500`/
`502`/`503`/`504`. Entre as tentativas espera um **backoff exponencial** (`base·2^n`, com teto
de 5s, pois estamos no caminho de uma mensagem viva). Cada retentativa sai na telemetria como
`[retentativa] <nome>: <falha> — retentando (X de Y) após Zms` e é contada pelo `bin/metricas`.

**O que NÃO retenta (de propósito):**

- `401`/`403` (auth) e `400`/`404`/`413`/`422` (erro da mensagem): repetir na hora daria o
  mesmo erro. Note que auth **conta** para o disjuntor mas **não** vale retentativa — as duas
  classificações divergem aqui de propósito (`indica_provedor_indisponivel` × `vale_retentar`).
- `Processo` (Claude via `claude --print`): um CLI caído não volta a si num respiro, e martelar
  o Claude é justamente o que evitamos (licao-refresh-token-rotativo).

**Desligado por padrão:** `retentativas` é por provedor e vale **0** quando ausente — uma
tentativa só, comportamento idêntico ao de antes, sem latência extra. Campos (por provedor,
opcionais): `retentativas` (0), `retentativa_espera_ms` (250).

## Orçamento de tempo total da cadeia (`orcamento_total_ms`)

**Problema que resolve:** cada provedor tem o seu `timeout`, mas nada limitava o tempo
**somado** da cadeia. Se o Claude trava até o timeout (ex.: 30s) e só então o Gemini é tentado
e depois o Ollama (mais ~35s), o usuário espera **mais de um minuto** por uma resposta de chat.
O orçamento total corta esse cenário.

**Como funciona:** o roteador marca o início da cadeia e, **antes de começar cada provedor**,
pergunta a `orcamento::deve_pular_por_orcamento` se o tempo já gasto NESTA mensagem passou do
limite. Se passou, o provedor de cima é **pulado** (sai `[orcamento] <nome>: orçamento total de
Xms esgotado (Yms decorridos) — indo direto ao piso`) e a cadeia segue rumo ao **piso**, que
**nunca é pulado** — a promessa "o robô nunca fica mudo" segue de pé.

**Complementa o disjuntor:** o disjuntor pula um provedor por **histórico** (vem falhando em
série, em mensagens anteriores); o orçamento pula por **tempo gasto NESTA mensagem** (mesmo um
provedor saudável que simplesmente demorou demais nesta rodada). Sinais diferentes, objetivo
convergente: depender menos da latência da cadeia de cima.

**Limitação honesta:** a checagem é **entre** provedores (antes de começar o próximo). Um
provedor **já iniciado** roda até o seu próprio `timeout` — não abortamos no meio da chamada
(exigiria cancelamento de I/O). O orçamento governa **quando parar de escalar**, não é um
relógio de parada rígido: o tempo total pode passar do orçamento pela duração do provedor em
curso.

**Desligado por padrão:** `orcamento_total_ms` é opcional e global. Ausente => sem orçamento =
comportamento antigo (nunca pula por tempo). Presente, é clampado para no mínimo `1` (um `0`
mandaria toda mensagem direto ao piso, quase sempre um engano). Exemplo: `"orcamento_total_ms":
25000` (não escale para provedores de cima depois de 25s já gastos nesta mensagem).

## Memória curta de conversa (`historico`)

**Problema que resolve:** cada mensagem era roteada **sem contexto** — o `Contexto.historico`
ia sempre vazio, então o modelo respondia como se nunca tivesse falado com você. Um "e o
segundo?" ou "explica melhor", que dependem do que veio ANTES, ficavam sem sentido.

**Como funciona:** quando ligada, a ponte guarda os últimos turnos de **cada chat** num arquivo
por chat (`<diretorio>/<chat>.json`) e os devolve como histórico na próxima mensagem. O ciclo
(no `ponte::processar`) é: **carregar** o histórico do disco → **rotear** com ele como contexto
→ ao responder com sucesso, **gravar** a nova troca (usuário + assistente).

**Cuidados de desenho (`historico.rs`):**

- **Limitado**: só os últimos `max_turnos` turnos, cada um truncado em `max_chars_por_turno` —
  memória curta não cresce sem teto (nem no disco, nem no tamanho/custo do prompt).
- **Um arquivo por chat**: chats diferentes nunca disputam o mesmo arquivo; a gravação é
  **atômica** (`arquivo::escrever_atomico`) para o caso de duas mensagens do mesmo chat quase
  juntas (a ponte atende cada update numa thread).
- **Degrada com graça**: arquivo ausente = conversa nova (histórico vazio); arquivo corrompido =
  histórico vazio + aviso no `stderr`. A memória é um EXTRA — jamais impede uma resposta. Só uma
  resposta **real** entra na memória (a de cortesia do erro NÃO entra, para não poluir o contexto).

**Desligada por padrão:** bloco `historico` opcional. Ausente/`habilitado:false` = a ponte nem
chama o módulo → comportamento idêntico ao de antes (risco zero na ponte viva). Para ligar:

```json
"historico": {
  "habilitado": true,
  "diretorio": "/var/log/roteador-historico",
  "max_turnos": 6,
  "max_chars_por_turno": 2000
}
```

Campos (todos opcionais, com padrões sensatos): `diretorio` (`/var/log/roteador-historico`),
`max_turnos` (6 = 3 trocas), `max_chars_por_turno` (2000).

## Provedor Claude CLI: leitura de resposta longa sem deadlock

O `ProvedorClaudeCli` fala com o `claude --print` por pipes de `stdin`/`stdout`. O buffer de
pipe do SO é pequeno (~64 KB): se a resposta do Claude passa disso e ninguém está **lendo** o
`stdout`, o processo **bloqueia escrevendo** e nunca termina. Ler o `stdout` só *depois* de o
processo sair (o padrão ingênuo) causa **deadlock** — e o roteador o mataria por falso
"timeout", perdendo uma resposta longa boa. Por isso cada cano ganha sua **própria thread**
(escrita do prompt, leitura de `stdout`, leitura de `stderr`), drenando em paralelo à espera do
`try_wait` com prazo. No timeout, o processo é morto e as threads se desprendem (nunca damos
`join` que pudesse travar o caminho da mensagem viva). Coberto por dois testes com um `claude`
**falso** (um script de shell temporário; o Claude real nunca é disparado): resposta de ~200 KB
volta inteira, e um processo lento é morto no prazo.

## Uso

```sh
cargo build --release
./target/release/roteador "qual a capital da França?"
# [provedor: ollama_local]
# Paris.
```

⚠️ **A config padrão começa pelo Claude.** Rodar `roteador "..."` sem override **dispara o
Claude** (`claude --print`) — o que contraria a regra de nunca acionar o Claude "só pra
testar" (`licao-refresh-token-rotativo`). Para exercitar o roteador sem tocar o Claude,
aponte a env `ROTEADOR_CONFIG` para uma config cuja ordem **não** tenha o Claude (ex.: só
Ollama, ou um piso `resposta_fixa`):

```sh
ROTEADOR_CONFIG=/tmp/so-ollama.json ./target/release/roteador "oi"
```

O binário da ponte já usava esse mesmo override; o `bin/roteador` agora também, por simetria.

## Métricas (`bin/metricas`)

O `rotear()` só ANEXA linhas cruas ao log (`/var/log/roteador-provedores.log`).
O `bin/metricas` faz o caminho inverso: LÊ o log e responde **"de quem o robô
realmente depende?"**. É só leitura — nunca dispara provedor, seguro rodar à vontade.

```sh
./target/release/metricas                    # log padrão
./target/release/metricas /outro/caminho.log # outro arquivo
# == Métricas do roteador de provedores ==
# - groq: 0 ok, 9 falha, 1 pulo, 0 cfg | —
# - ollama_local: 6 ok, 42 falha, 0 pulo, 0 cfg | 7310ms média (p50 363 / p95 38603 / máx 38603)
# total de roteamentos: 6
# caiu no piso (Ollama): 6 de 6 (100.0%)
```

A linha-chave é a última: **quantas vezes caímos no piso (Ollama)**. Quanto maior o %,
mais o robô está rodando sem provedor bom — sinal pra investigar Claude/Groq/Gemini.

### Falhas por motivo (o "por quê" da queda)

A contagem de `falha` diz **quanto**; ela sozinha não diz **por quê**. Quando um provedor
tem falhas, o relatório abre uma linha com o motivo, deduzido do log:

```
- claude: 3 ok, 12 falha, 0 pulo, 0 cfg | 850ms média
    ↳ falhas por motivo: auth 9, timeout/5xx 3
falhas por motivo (total): auth 9, timeout/5xx 3, rede 2
```

É o sinal **mais acionável** do relatório: o Claude falhando por **`auth`** é o token
rotativo caindo — problema de credencial, não de rede; falhando por **`timeout/5xx`** é
lentidão/instabilidade do serviço. Dois problemas diferentes que a contagem crua confunde.
Categorias:

| motivo (`rótulo` / `chave` JSON)        | de onde vem                                  |
|-----------------------------------------|----------------------------------------------|
| `auth` / `autenticacao`                 | HTTP 401/403 **ou** o `claude --print` reclamando de login/token (credencial recusada) |
| `rate-limit` / `limite_taxa`            | HTTP 429 (estourou a cota)                    |
| `timeout/5xx` / `servidor_instavel`     | HTTP 408 ou 5xx (servidor sobrecarregado)     |
| `rede` / `rede`                         | conexão recusada, host fora, socket estourou |
| `processo` / `processo`                 | `claude --print` falhou/travou                |
| `req-inválida` / `requisicao_invalida`  | HTTP 400/404/413/422 (problema DESTA mensagem)|
| `resposta-ruim` / `resposta_ruim`       | respondeu vazio ou em formato inesperado      |
| `config` / `configuracao`               | desabilitado / sem chave / sem url_base       |
| `outra` / `outra`                       | motivo não reconhecido (formato antigo)       |

A soma das categorias de um provedor sempre bate com o total de `falha` dele. No `--json`,
cada provedor ganha um objeto `falhas_por_categoria` (chaves estáveis em snake_case) e há
um agregado de mesmo nome no topo. Como toda a métrica, é **só leitura** — jamais dispara
provedor (o Claude nunca é tocado).

**Auth do Claude CLI:** o `claude --print` é opaco — quando o token OAuth cai (a dor #1 do
projeto), ele apenas sai com código != 0 e imprime um erro. O provedor Claude olha esse
texto de erro e, se reconhecer marcadores de credencial (`authentication`, `unauthorized`,
`/login`, `token expired`, `401`…), classifica a falha como **`autenticação`** em vez do
genérico `processo` — assim o token caindo aparece como **`auth`** aqui, não escondido em
`processo`. A heurística é conservadora: o que não casar continua `processo`, e o
comportamento de roteamento é **idêntico** nos dois casos (auth e processo contam pro
disjuntor e não valem retentativa) — só muda o rótulo da telemetria. Ver
`provedor::parece_falha_de_autenticacao`.

### Latência: média + percentis (performance)

A latência aparece por provedor. Com **2 ou mais respostas**, além da média mostramos
**p50 / p95 / máx** — porque a **média mente**: no exemplo acima o Ollama tem "7310ms
média", mas o p50 é 363ms (metade das respostas é rápida) e o p95/máx revela uma travada
de **38,6 s** que a média dilui. O p95 é "quão ruim fica nos piores casos" (o que dói pro
usuário esperando); o máx é o pior caso absoluto. Com 1 só resposta, os percentis seriam
iguais à média (ruído), então mostramos só a média. Método: *nearest-rank* sobre as
latências ordenadas (didático e sem dependência).

### Pulos por disjuntor (economia)

Quando o [disjuntor](#disjuntor--circuit-breaker) está ligado e abre o circuito de um
provedor, o `rotear()` PULA esse provedor e loga `[disjuntor] <nome>: ... — pulando`.
As métricas contam esses pulos por provedor (coluna `N disjuntor`, só aparece quando há
algum) e somam numa linha-resumo. **Cada pulo é a latência de um provedor morto que a
cadeia NÃO pagou** — é a economia do disjuntor virando número:

```
# - claude: 0 ok, 0 falha, 0 pulo, 0 cfg, 8 disjuntor | —
# ...
# provedores pulados por disjuntor (latência de morto evitada): 8
```

Com o disjuntor desligado (padrão) não há pulos e essas linhas nem aparecem.

### Disjuntor em sombra (decisão de ligar viralizada em número)

Quando o disjuntor roda em [modo sombra](#modo-sombra-sombra-true--a-rampa-de-confiança), ele
loga por mensagem o que FARIA (`[disjuntor-sombra] ...`). As métricas AGREGAM essas linhas para
transformar a decisão de ligar o disjuntor em números de uma olhada, em vez de `grep` linha a linha:

```
# disjuntor em sombra: pularia 2 certo(s) (~8100ms poupados), 0 falso(s) positivo(s)
#   → sem falsos positivos: candidato a ligar o disjuntor
```

- **pularia N certo(s)**: pulos que o disjuntor ativo acertaria (previu falha e o provedor falhou).
- **~Xms poupados**: a latência de provedor morto que ele teria poupado — a economia projetada
  sobre o tráfego real, ANTES de ligar.
- **M falso(s) positivo(s)**: vezes que pularia um provedor que na verdade RESPONDEU. `M = 0` →
  "candidato a ligar"; `M > 0` → "NÃO ligar ainda / subir limiar/cooldown". É o freio.

No `--json`: campos `sombra_pularia_ok`, `sombra_economia_ms`, `sombra_falsos_positivos` por
provedor e no topo. Sem atividade de sombra (caso comum), nada disso aparece.

### Retentativas (instabilidade absorvida)

Quando a [retentativa](#retentativa-em-falhas-transitórias-retentativas) está ligada e um
provedor sofre um blip transitório, o `rotear()` loga `[retentativa] <nome>: ... — retentando
(X de Y) após Zms`. As métricas contam essas retentativas por provedor (coluna `N retentativa`,
só aparece quando há alguma) e somam numa linha-resumo. **Cada retentativa é um blip passageiro
que a cadeia absorveu** em vez de cair pro piso — quanto mais alto, mais instável a rede ou os
provedores remotos andaram:

```
# - groq: 5 ok, 0 falha, 0 pulo, 0 cfg, 3 retentativa | 900ms média
# ...
# retentativas transitórias (blips absorvidos): 3
```

Com a retentativa desligada (padrão) não há retentativas e essas linhas nem aparecem.

### Frescor: há quanto tempo cada provedor bom respondeu

A **sequência no piso** conta EVENTOS (quantos roteamentos seguidos caíram no Ollama). Mas
um bot de pouco tráfego pode ter sequência 3 e estar sem provedor bom **há horas** — 3
mensagens em 6 horas. O **frescor** fecha esse buraco medindo dependência em **tempo de
parede**: por provedor, há quanto tempo ele respondeu pela última vez, e a linha-chave
**"sem provedor bom há X"** (tempo desde o último sucesso FORA do piso):

```
# -- frescor (último sucesso por provedor) --
# - claude: há 6h15min (2026-06-30 22:19:34 UTC)
# - ollama_local: há 4h27min (2026-07-01 00:08:08 UTC)
# sem provedor bom há 6h15min
```

Enquanto a cadeia de cima estiver caindo, esse número **cresce em tempo real**. Se nenhum
provedor bom respondeu no período, a linha vira "nenhum provedor bom respondeu (só o piso)".
A seção só aparece quando há pelo menos um sucesso datado, e some se o relógio do sistema
estiver quebrado (o resto do relatório segue válido). No `--json`, cada provedor traz
`ultimo_sucesso_epoch` e o topo traz `ultimo_sucesso_fora_do_piso_epoch` (epochs absolutos —
a máquina calcula a idade sozinha, sem depender de "agora").

### Volume (caracteres processados / tokens estimados)

O custo por resposta (abaixo) trata toda troca como igual, mas uma resposta de 20 chars e
outra de 4000 chars pesam MUITO diferente. Para fechar essa dimensão, cada `[ok]` passou a
registrar o **volume da troca** — `(entrada ~N chars, resposta ~M chars)` — e o relatório
agrega, por provedor, quanto texto entrou e saiu, mais um proxy de tokens (≈ chars/4):

```
-- volume (caracteres processados) --
- claude: 1060 chars (entrada 40, resposta 1020) ~265 tokens
- ollama_local: 105 chars (entrada 15, resposta 90) ~26 tokens
volume total: 1165 chars (entrada 55, resposta 1110)
```

- **Só aparece quando há volume registrado** (logs novos); em logs antigos a seção some e os
  campos ficam 0 — retrocompatível, sem regressão.
- Mede o volume da **troca** (mensagem do usuário + resposta), *não* o prompt completo em
  tokens: o preâmbulo de sistema e o histórico montados dentro de cada provedor não entram.
  Os `~tokens` são uma **estimativa grosseira** (chars/4), ordem de grandeza — não a fatura.
- No `--json`: cada provedor ganha `chars_entrada`/`chars_resposta`/`tokens_estimados`, e o
  topo traz `chars_entrada`/`chars_resposta` agregados.
- Continua **só leitura** do log: nunca dispara provedor.

### Custo estimado (`--custo`)

A dependência também tem **preço**. Passe `--custo <provedor>=<valor>` (repetível) com o
custo por resposta de cada provedor pago — na unidade que você quiser (centavos, dólares,
créditos) — e o relatório fecha com o custo estimado no período:

```sh
./target/release/metricas --janela 24h --custo claude=3 --custo gemini=0.5
# ... (relatório normal acima) ...
# -- custo estimado (por resposta) --
# - claude: 3.00
# - ollama_local: 0.00 (sem preço → 0)
# custo total estimado: 3.00
```

Quem não tem preço entra como **0** (ex.: o piso Ollama, local e grátis), marcado para a
conta ficar transparente. Sem nenhum `--custo`, a seção nem aparece (compatível com o uso
antigo). **Limitação honesta:** o custo aqui é **por resposta** (todas contam igual). Para
levar o TAMANHO da troca em conta, use `--custo-por-mil-tokens` (abaixo) ou veja a seção
**Volume** acima.

### Custo estimado por token (`--custo-por-mil-tokens`)

Como os provedores de verdade cobram por **token**, não por resposta, este flag chega mais
perto da fatura real do que o `--custo` (por resposta, que ignora o tamanho da troca). Passe
`--custo-por-mil-tokens <provedor>=<valor>` (repetível) com o preço por **mil tokens
estimados**; o relatório multiplica pelos tokens estimados de cada provedor (≈ `chars/4`, do
**Volume** acima) e fecha com o total:

```sh
./target/release/metricas --janela 24h --custo-por-mil-tokens claude=2
# ... (relatório normal acima) ...
# -- custo estimado (por mil tokens ~chars/4) --
# - claude: 0.65
# - ollama_local: 0.00
# custo total estimado (por token): 0.65
```

É **ortogonal** ao `--custo`: pode passar um, o outro, os dois (as seções aparecem
separadas) ou nenhum. No `--json`, o bloco vira `custo_por_token` (irmão de `custo`), cada um
só presente se houver o preço correspondente. **Limitação honesta:** os tokens são
**estimados** por `chars/4` (não a contagem real do tokenizador do provedor) e o volume é o da
troca (mensagem + resposta), sem o sistema/histórico que o provedor monta por dentro — é
aproximação de custo, não a fatura exata.

### Saída para máquina (`--json`)

O relatório de texto é para o humano ler. Com `--json`, o **mesmo conteúdo** sai como um
objeto JSON em uma linha — para um dashboard, um alerta externo ou outro programa consumir
sem ter que parsear texto solto:

```sh
./target/release/metricas --json
# {"total_roteamentos":7,"caiu_no_piso":6,"percentual_no_piso":85.71...,
#  "sequencia_atual_no_piso":3,"maior_sequencia_no_piso":3,"pulos_disjuntor":25,
#  "linhas_ignoradas":12,"provedores":{"claude":{"sucessos":1,...,"latencia_media_ms":42322,
#  "latencia_p50_ms":42322,"latencia_p95_ms":42322,"latencia_maxima_ms":42322}, ...}}
```

O `--json` combina com `--janela` e `--custo` (o bloco `custo` só entra se houver `--custo`,
igual ao relatório de texto). Detalhes que valem notar:

- Latência de um provedor que **nunca respondeu** vira `null`, não `0` — `0ms` seria mentira.
- A saída é **só** o JSON (nada de texto humano em volta), para continuar sendo JSON válido.
- Continua **só leitura** do log: nunca dispara provedor. Serializado pelo nosso próprio
  codificador JSON (`json.rs`), zero dependências.

```sh
# Ex.: extrair o percentual no piso das últimas 24h com jq
./target/release/metricas --json --janela 24h | jq .percentual_no_piso
```

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
   provedor bom responde, a sequência zera e o estado é limpo. A mensagem é **escalonada por
   severidade** (`alerta::severidade`): cada degrau de `limite` quedas sobe o nível —
   🟡 **ATENÇÃO** (`[limite, 2×limite)`) → 🟠 **ALERTA** (`[2×limite, 3×limite)`) →
   🔴 **CRÍTICO** (`≥ 3×limite`). Como o re-alerta também dispara a cada degrau, cada nova
   notificação chega com a gravidade mais alta que a anterior — o Thiago vê a degradação
   crescer mensagem a mensagem, sem flood.
2. **Percentual** — **fração alta** de quedas no piso na janela, *mesmo sem quedas em série*
   (cadeia falhando de forma intermitente mas pesada — ex.: 8 de 10 roteamentos no piso, sem
   nunca acumular 5 seguidas). Função pura `alerta::decidir_por_percentual`. Pega o que o alarme
   de sequência deixa passar. Anti-spam com **histerese**: avisa ao cruzar o limiar, fica quieto
   enquanto continua alto e só re-arma quando a fração cai com folga (`limiar − 15` pontos),
   evitando ligar/desligar na fronteira. Exige um **mínimo de amostras** para não alertar com
   pouca evidência (ex.: "1 de 1 = 100%"). Também **escalonado por severidade**
   (`alerta::severidade_percentual`), a cada 10 pontos acima do limiar: 🟡 **ATENÇÃO** (70–79%) →
   🟠 **ALERTA** (80–89%) → 🔴 **CRÍTICO** (≥ 90%). Mesma escada visual do alarme de sequência,
   então as duas mensagens "falam a mesma língua" de gravidade.

**Escalada de urgência em CRÍTICO** (`alerta::escalonar_por_severidade` /
`escalonar_percentual_por_severidade`): no nível 🔴 **CRÍTICO** os dois alarmes mudam de
comportamento, porque um silêncio longo numa situação grave é pior que uma repetição. (1) A
mensagem abre com o banner **🚨 URGENTE 🚨** (`alerta::prefixo_urgencia`) — o sinal mais forte
que o canal de texto permite, já que o `notificar-thiago.sh` não tem prioridade nativa. (2) O
anti-spam é **furado**: em vez de esperar o próximo degrau (sequência) ou ficar preso na histerese
(percentual), o alarme **re-avisa a cada rodada do cron** enquanto seguir crítico, mantendo o
estado coerente para o anti-spam normal voltar a valer assim que de-escalar. Abaixo de CRÍTICO
nada muda — ATENÇÃO/ALERTA seguem o anti-spam por degraus/histerese, sem flood.

Ambos só LÊEM o log — nunca disparam provedor → não tocam o Claude.

```sh
./target/release/alerta --simular              # decide e imprime, NÃO manda Telegram nem grava estado
./target/release/alerta --limite 5 --limiar-percentual 70 --janela 24h
# [alerta] seq_no_piso=3 limite=5 ja_alertado=0 severidade=NORMAL -> notificar=false novo_estado=0
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

## Verificar config (`bin/verificar-config`)

Toda a garantia do projeto — "o robô **nunca fica mudo** porque o piso (Ollama local)
responde quando o resto falha" — depende de uma config bem-formada. Se alguém desabilita o
piso, tira ele da ordem, põe um provedor que exige chave como último, ou cita na
`ordem_fallback` um nome inexistente, a garantia **quebra em silêncio**: só se descobre em
produção, quando a cadeia inteira cai e o `rotear()` devolve `TodosFalharam` — com o bot vivo
e o usuário mudo.

O `bin/verificar-config` pega essa classe de erro **antes do deploy**. Roda a verificação
estática (funções puras de `verificacao.rs`) sobre a config já parseada: **nenhuma rede,
nenhum processo, nenhum provedor construído para valer** — 100% seguro (jamais toca o Claude).

```sh
verificar-config                        # confere o arquivo padrão (/root/.secrets/…)
verificar-config /tmp/outra-config.json # confere outro arquivo
```

Distingue dois graus: **❌ ERRO** (quebra o roteamento ou a garantia do piso — precisa
corrigir) e **⚠️ AVISO** (funciona, mas quase certamente é engano ou desperdício). O que ele
checa:

| Achado | Grau |
| --- | --- |
| `ordem_fallback` vazia | erro |
| nome na ordem sem provedor declarado | erro |
| tipo de provedor desconhecido | erro |
| campo obrigatório faltando (ollama sem `url_base`/`modelo`, `resposta_fixa` sem `mensagem_fixa`, etc.) | erro |
| **piso (último) desabilitado** | erro |
| **piso do tipo que exige chave externa** (`gemini_rest`, ou `openai_compat` com `url_base` https) | erro |
| piso do tipo que não é `ollama`/`resposta_fixa` (ex.: claude como último) | aviso |
| nome repetido na ordem | aviso |
| provedor declarado fora da ordem (nunca usado) | aviso |
| provedor habilitado sem `chave` (será pulado sempre) | aviso |

Saída de processo: **0** sem erros (pode ter avisos), **1** com pelo menos um erro (ou falha
ao ler/parsear). Útil em cron/CI: `verificar-config && deploy`. Exemplo contra uma config
quebrada:

```
❌ ERRO   'fantasma' está na ordem_fallback mas não foi declarado em 'provedores'
❌ ERRO   provedor 'xpto': tipo 'inventado' desconhecido (o roteador vai pular sempre)
❌ ERRO   piso 'ollama_local' (último da ordem) está DESABILITADO: se toda a cadeia falhar, o robô fica mudo

Resumo: 3 erros e 1 aviso.
```

## Diagnóstico do piso (`bin/diagnostico`)

O `verificar-config` é um doutor **estático**: lê a config e aponta erros no papel. Mas há um
buraco que só a realidade revela — **e se o próprio piso (Ollama) estiver fora do ar?** Toda a
garantia do projeto ("o robô nunca fica mudo") depende do último provedor da cadeia responder.
Os alarmes do `bin/alerta` medem "caí no piso DEMAIS" — mas *assumem* que o piso responde. Se o
Ollama cair, o robô fica mudo e nenhum alarme existente pega isso.

O `bin/diagnostico` é o doutor **vivo** desse ponto único de falha. Ele:

- identifica o piso (o último da `ordem_fallback`);
- faz uma checagem **barata**: `GET /api/tags` do Ollama — só **lista** os modelos instalados,
  **não roda inferência** (não paga os ~33s de uma geração);
- confirma que o **modelo configurado está instalado** (senão a geração falharia);
- é **seguro**: só sonda piso do tipo `ollama` (HTTP local, custo zero, sem token). Se o piso
  for outro tipo (Claude/pago), **recusa sondar de propósito** — jamais dispara o Claude "pra
  testar" (ver `licao-refresh-token-rotativo`).

```
diagnostico                         # sonda o piso da config padrão (/root/.secrets/…)
diagnostico /tmp/outra-config.json  # sonda o piso de outra config
```

Código de saída (útil em cron: `diagnostico || avisar-thiago`):

| Saída | Significado |
|-------|-------------|
| **0** | piso vivo e com o modelo certo instalado (ou piso `resposta_fixa` com texto → sempre vivo) |
| **1** | piso **comprometido**: fora do ar, sem o modelo, desabilitado, ou inexistente |
| **2** | não deu para verificar (piso não é Ollama nem `resposta_fixa` → não sondamos) |

Exemplos reais (provados ao vivo, Claude jamais tocado):

```
✅ Piso 'ollama_local' vivo e com o modelo 'qwen2.5:1.5b' instalado.
❌ Piso 'ollama_local' respondeu, mas o modelo 'llama3:70b' NÃO está instalado. Instalados: qwen2.5:1.5b, tinyllama:latest.
❌ Piso 'piso' fora do ar: rede: falha ao conectar em 127.0.0.1:1: Connection refused (os error 111).
❌ Piso 'piso' está DESABILITADO na config — o robô fica mudo se a cadeia cair.
⚠️ Piso 'claude' é do tipo 'claude_cli' (não-Ollama); não sondei para não disparar provedor pago/Claude. Vivacidade NÃO confirmada.
```

A decisão é feita por funções **puras** (`avaliar_resposta_tags`, `modelo_presente`,
`nomes_dos_modelos`), testáveis sem rede; só `verificar_piso` abre o socket.

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
6. enviar_mensagem(token, chat, texto)  (sendMessage HTTPS via curl; reenvia em falha transitória)
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

### Reenvio ao Telegram em falha transitória

O passo 6 (`enviar_mensagem` → `sendMessage`) **reenvia** quando a entrega falha de forma
transitória. Motivo: a resposta já foi **gerada** — às vezes com uma chamada cara ao Claude — e
perdê-la para um blip do Telegram deixaria o usuário mudo com uma resposta boa na mão. É a
promessa central do projeto ("o robô nunca fica mudo") estendida da **geração** para a **entrega**.

- **Só reenvia quando o servidor REJEITOU** (HTTP `429`/`5xx`/`408`): nesses casos temos certeza
  de que a mensagem **não** foi entregue, então reenviar não duplica.
- **Não reenvia falha de rede** (`Rede`): é ambígua — a mensagem pode ter chegado antes de a
  conexão cair — então preferimos não arriscar mandar a resposta duas vezes.
- **Não reenvia `400`/`404`** (mensagem malformada): repetir não conserta; desiste na hora.
- Num `429` o Telegram costuma dizer **quanto** esperar (`parameters.retry_after`); quando diz,
  honramos esse tempo (limitado por um teto de 8s para não travar a thread). Sem `retry_after`,
  cai no backoff exponencial (500→1000ms), até `MAX_RETENTATIVAS_ENVIO` (2) reenvios.

A **decisão** é a função pura `espera_reenvio`; a **execução** (enviar/dormir/logar) é injetada
em `enviar_com_politica`, testada com mocks (sem tocar rede, relógio nem log). O caminho feliz
(entrega de primeira) é idêntico ao de antes — o reenvio só entra quando a entrega falha.

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
verificar-config                                        # confere a config de produção antes do deploy
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
