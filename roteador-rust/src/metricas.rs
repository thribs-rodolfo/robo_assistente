//! Métricas agregadas de telemetria: lê o log do roteador e responde
//! "de quem o robô realmente depende?".
//!
//! O módulo [`telemetria`](crate::telemetria) só ANEXA linhas cruas ao log
//! (`/var/log/roteador-provedores.log`). Aqui fazemos o caminho inverso: LEMOS essas
//! linhas e somamos, por provedor, quantas vezes cada um respondeu, falhou ou foi pulado,
//! e a latência média de quem respondeu. O número que mais importa no fim é
//! "quantas vezes caímos no Ollama" — quanto maior, mais o robô está sem provedor bom.
//!
//! Filosofia (WORKSPACE_RULES "Como escrevemos código"): zero dependências (parsing à mão),
//! agnóstico (qualquer nome de provedor entra no mapa), testável (toda a lógica de parsing
//! é função pura sobre `&str`) e sem erro silencioso (a leitura do arquivo devolve `Result`).

use std::collections::BTreeMap;

/// Contagens e latência acumuladas de UM provedor, lidas do log.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MetricasProvedor {
    /// Quantas vezes este provedor respondeu com sucesso (`[ok] respondido por '<nome>'`).
    pub sucessos: u64,
    /// Quantas vezes este provedor foi tentado e falhou (`[falha] <nome>: ...`).
    pub falhas: u64,
    /// Quantas vezes foi pulado na pré-checagem (`[pula] <nome>: ...` — desabilitado/sem chave).
    pub pulos: u64,
    /// Problemas de configuração (na ordem sem config, tipo desconhecido).
    pub problemas_config: u64,
    /// Quantas vezes este provedor foi PULADO pelo disjuntor aberto
    /// (`[disjuntor] <nome>: disjuntor aberto (...) — pulando`). Cada pulo é a latência de
    /// um provedor morto que a cadeia NÃO pagou — a economia que o disjuntor entrega.
    pub disjuntor_pulos: u64,
    /// Quantas RE-tentativas transitórias este provedor sofreu (`[retentativa] <nome>: ...`).
    /// É a atividade da retentativa: quanto maior, mais blips passageiros o provedor teve que
    /// engolir antes de responder (ou de a cadeia cair pro próximo). Sinal de instabilidade
    /// que a contagem de sucessos/falhas sozinha esconde.
    pub retentativas: u64,
    /// Modo SOMBRA do disjuntor — vezes que o disjuntor ATIVO PULARIA este provedor e a
    /// previsão estava CERTA (ele de fato falhou): `[disjuntor-sombra] pularia '<nome>' ...`.
    /// Cada uma é um pulo que o disjuntor ligado teria dado sem prejuízo. Ver [`crate::disjuntor`].
    pub sombra_pularia_ok: u64,
    /// Soma (ms) da economia ESTIMADA dessas previsões certas: a latência real que cada
    /// provedor morto gastou e que o disjuntor ligado teria poupado. É a economia do disjuntor
    /// projetada sobre o tráfego real, ANTES de ligá-lo — a base pra decidir se vale ligar.
    pub sombra_economia_ms: u128,
    /// Modo SOMBRA — vezes que o disjuntor ATIVO PULARIA este provedor mas ele RESPONDEU
    /// (FALSO POSITIVO): `[disjuntor-sombra] PULARIA '<nome>' ... mas ele RESPONDEU ...`.
    /// Qualquer número acima de 0 é sinal de que ligar o disjuntor agora custaria respostas
    /// boas — é o freio que impede ligar cedo demais.
    pub sombra_falsos_positivos: u64,
    /// Latência (ms) de CADA resposta com sucesso, na ordem em que apareceram no log.
    /// Guardamos a lista inteira (não só a soma) para poder tirar tanto a média quanto os
    /// percentis (p50/p95) e o máximo — a média sozinha esconde a "cauda" (o provedor que
    /// costuma ir bem mas às vezes trava). É o sinal de performance que o goal pede.
    pub latencias_ms: Vec<u128>,
    /// Instante (epoch UTC, segundos) do ÚLTIMO sucesso deste provedor com carimbo legível,
    /// ou `None` se ele nunca respondeu (ou só respondeu em linhas sem timestamp). É a base do
    /// "frescor": há quanto tempo cada provedor de fato funcionou pela última vez. Mede
    /// dependência em TEMPO DE PAREDE — complementa a sequência, que conta EVENTOS: um bot com
    /// pouco tráfego pode ter sequência 3 mas estar sem provedor bom há horas.
    pub ultimo_sucesso_epoch: Option<u64>,
    /// Quantas falhas de cada CATEGORIA este provedor teve (auth, rate-limit, timeout, rede,
    /// processo...). A contagem de `falhas` sozinha diz QUANTO, não POR QUÊ; isto abre o "por
    /// quê". É o sinal mais acionável do relatório: o Claude falhando por AUTENTICAÇÃO é o token
    /// rotativo caindo ([[licao-refresh-token-rotativo]]); falhando por TIMEOUT é lentidão — dois
    /// problemas diferentes que a contagem crua confunde. A soma dos valores == `falhas`.
    pub falhas_por_categoria: BTreeMap<CategoriaFalha, u64>,
}

/// Por que um provedor falhou, deduzido do motivo que o [`crate::lib`] gravou no log.
///
/// Cada `[falha] <nome>: <motivo> ...` carrega o [`Display`](std::fmt::Display) de uma
/// [`FalhaProvedor`](crate::erro::FalhaProvedor) (`http 401: ...`, `rede: ...`, `processo:
/// ...`). Aqui traduzimos esse motivo de volta para uma categoria estável, para o relatório
/// responder "de que MORREU cada provedor". É telemetria só-leitura: nada dispara provedor.
///
/// `Ord`/`Eq` derivados para servir de chave de `BTreeMap` (saída ordenada e determinística).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CategoriaFalha {
    /// Autenticação recusada (HTTP 401/403). No Claude, é o sintoma do token rotativo caindo.
    Autenticacao,
    /// Limite de taxa estourado (HTTP 429): bateu na cota momentânea do provedor.
    LimiteTaxa,
    /// Timeout (HTTP 408) ou erro do servidor (5xx): provedor sobrecarregado/instável.
    ServidorInstavel,
    /// Rede/conexão: provedor fora do ar, host inacessível, socket estourou.
    Rede,
    /// Processo externo (`claude --print`) falhou: não encontrado, código != 0, travou/timeout.
    Processo,
    /// Requisição rejeitada pelo conteúdo (HTTP 400/404/413/422): problema DESTA mensagem, não
    /// do provedor (ele está de pé). Separada de propósito — não indica dependência do piso.
    RequisicaoInvalida,
    /// O provedor respondeu, mas vazio ou em formato inesperado (falha de contrato, não de saúde).
    RespostaRuim,
    /// Pré-checagem/config reprovou (desabilitado, sem chave, sem url_base).
    Configuracao,
    /// Motivo não reconhecido (formato antigo/desconhecido). Nunca deveria dominar — se dominar,
    /// é sinal de que o formato do log mudou e este parser precisa acompanhar.
    Outra,
}

impl CategoriaFalha {
    /// Rótulo curto para o relatório em texto (ex.: `auth`, `timeout/5xx`).
    pub fn rotulo(&self) -> &'static str {
        match self {
            CategoriaFalha::Autenticacao => "auth",
            CategoriaFalha::LimiteTaxa => "rate-limit",
            CategoriaFalha::ServidorInstavel => "timeout/5xx",
            CategoriaFalha::Rede => "rede",
            CategoriaFalha::Processo => "processo",
            CategoriaFalha::RequisicaoInvalida => "req-inválida",
            CategoriaFalha::RespostaRuim => "resposta-ruim",
            CategoriaFalha::Configuracao => "config",
            CategoriaFalha::Outra => "outra",
        }
    }

    /// Chave estável (snake_case, sem acento/espaço) para o JSON — boa para consumo por máquina.
    pub fn chave(&self) -> &'static str {
        match self {
            CategoriaFalha::Autenticacao => "autenticacao",
            CategoriaFalha::LimiteTaxa => "limite_taxa",
            CategoriaFalha::ServidorInstavel => "servidor_instavel",
            CategoriaFalha::Rede => "rede",
            CategoriaFalha::Processo => "processo",
            CategoriaFalha::RequisicaoInvalida => "requisicao_invalida",
            CategoriaFalha::RespostaRuim => "resposta_ruim",
            CategoriaFalha::Configuracao => "configuracao",
            CategoriaFalha::Outra => "outra",
        }
    }
}

impl MetricasProvedor {
    /// Latência média (ms) das respostas com sucesso, ou `None` se nunca respondeu.
    pub fn latencia_media_ms(&self) -> Option<u128> {
        if self.latencias_ms.is_empty() {
            None
        } else {
            let soma: u128 = self.latencias_ms.iter().sum();
            Some(soma / self.latencias_ms.len() as u128)
        }
    }

    /// Percentil `p` (0–100) das latências de sucesso, em ms, ou `None` se nunca respondeu.
    ///
    /// Método "nearest-rank" (o mais simples e didático): ordena uma cópia das latências e
    /// pega o elemento de posição `ceil(p/100 · n)`. Ex.: com 20 amostras, o p95 é a 19ª
    /// (as duas mais lentas ficam acima). Diferente da média, o p95 mostra "quão ruim fica
    /// nos piores casos" — é o que dói para o usuário esperando resposta.
    pub fn latencia_percentil(&self, p: u8) -> Option<u128> {
        if self.latencias_ms.is_empty() {
            return None;
        }
        let mut ordenado = self.latencias_ms.clone();
        ordenado.sort_unstable();
        let n = ordenado.len();
        // rank vai de 1 a n; o índice do vetor é rank-1. `p=0` cai no menor (índice 0).
        let rank = ((p as f64 / 100.0) * n as f64).ceil() as usize;
        let indice = rank.saturating_sub(1).min(n - 1);
        Some(ordenado[indice])
    }

    /// Maior latência (ms) já vista neste provedor, ou `None` se nunca respondeu.
    /// É o pior caso absoluto — útil para flagrar travadas raras que nem o p95 pega.
    pub fn latencia_maxima_ms(&self) -> Option<u128> {
        self.latencias_ms.iter().copied().max()
    }

    /// Resumo das falhas por categoria em uma linha (ex.: `auth 3, rede 2`), ou string vazia
    /// quando não houve falha. Itera o `BTreeMap` (ordenado pela ordem do enum) → saída estável.
    pub fn resumo_falhas(&self) -> String {
        self.falhas_por_categoria
            .iter()
            .map(|(cat, n)| format!("{} {n}", cat.rotulo()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Relatório agregado de todo o log: um mapa de provedor -> métricas, ordenado por nome.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Relatorio {
    /// Métricas por provedor (BTreeMap = sempre ordenado por nome, saída determinística).
    pub por_provedor: BTreeMap<String, MetricasProvedor>,
    /// Linhas que não casaram com nenhum padrão conhecido (ruído/formato antigo).
    pub linhas_ignoradas: u64,
    /// Quantos dos roteamentos MAIS RECENTES, em sequência, caíram no piso (Ollama).
    /// Zera assim que um provedor bom responde. É o alarme de "estou sem provedor bom AGORA":
    /// se está alto, a cadeia toda de cima vem falhando em série.
    pub sequencia_atual_no_piso: u64,
    /// A maior sequência de roteamentos seguidos no piso já vista no período analisado.
    pub maior_sequencia_no_piso: u64,
}

impl Relatorio {
    /// Total de roteamentos concluídos = soma dos sucessos de todos os provedores.
    /// (Cada roteamento termina em exatamente um `[ok]`, enquanto o Ollama segura o piso.)
    pub fn total_roteamentos(&self) -> u64 {
        self.por_provedor.values().map(|m| m.sucessos).sum()
    }

    /// Total de pulos por disjuntor somando todos os provedores — a economia agregada
    /// (quantas vezes, no período, a cadeia deixou de pagar a latência de um provedor morto).
    pub fn total_pulos_disjuntor(&self) -> u64 {
        self.por_provedor.values().map(|m| m.disjuntor_pulos).sum()
    }

    /// Total de retentativas transitórias somando todos os provedores — quantos blips
    /// passageiros a retentativa absorveu no período (quanto mais alto, mais instável a rede
    /// ou os provedores remotos andaram).
    pub fn total_retentativas(&self) -> u64 {
        self.por_provedor.values().map(|m| m.retentativas).sum()
    }

    /// Falhas por categoria somando TODOS os provedores — "de que os provedores morreram no
    /// período, no agregado". Complementa a visão por provedor: aqui vê-se, por exemplo, que
    /// metade de todas as falhas foi de autenticação (token) sem precisar somar de cabeça.
    pub fn falhas_por_categoria_total(&self) -> BTreeMap<CategoriaFalha, u64> {
        let mut total: BTreeMap<CategoriaFalha, u64> = BTreeMap::new();
        for m in self.por_provedor.values() {
            for (cat, n) in &m.falhas_por_categoria {
                *total.entry(*cat).or_insert(0) += *n;
            }
        }
        total
    }

    /// Total de pulos que o disjuntor em modo SOMBRA acertaria (previsão certa) no período.
    pub fn total_sombra_pularia_ok(&self) -> u64 {
        self.por_provedor
            .values()
            .map(|m| m.sombra_pularia_ok)
            .sum()
    }

    /// Economia (ms) ESTIMADA total do disjuntor no modo sombra — quanto de latência de
    /// provedor morto ele teria poupado no período se estivesse LIGADO. É a projeção da
    /// economia sobre o tráfego real, o número que ajuda a decidir se vale ligar o disjuntor.
    pub fn total_sombra_economia_ms(&self) -> u128 {
        self.por_provedor
            .values()
            .map(|m| m.sombra_economia_ms)
            .sum()
    }

    /// Total de FALSOS POSITIVOS do modo sombra — vezes que o disjuntor pularia um provedor
    /// que na verdade respondeu. Qualquer valor > 0 desaconselha ligar o disjuntor sem afinar.
    pub fn total_sombra_falsos_positivos(&self) -> u64 {
        self.por_provedor
            .values()
            .map(|m| m.sombra_falsos_positivos)
            .sum()
    }

    /// Quantas vezes caímos no provedor-piso (qualquer nome contendo "ollama").
    /// É a métrica-chave de dependência: subir muito = provedores bons estão caindo.
    pub fn sucessos_no_piso(&self) -> u64 {
        self.por_provedor
            .iter()
            .filter(|(nome, _)| eh_piso(nome))
            .map(|(_, m)| m.sucessos)
            .sum()
    }

    /// Fração (0.0–100.0) dos roteamentos que caíram no piso (Ollama). 0 quando não houve
    /// nenhum roteamento. É a dependência expressa em porcentagem — base tanto do relatório
    /// legível quanto do alarme percentual ([`crate::alerta::decidir_por_percentual`]).
    pub fn percentual_no_piso(&self) -> f64 {
        let total = self.total_roteamentos();
        if total == 0 {
            0.0
        } else {
            (self.sucessos_no_piso() as f64 / total as f64) * 100.0
        }
    }

    /// Custo estimado por provedor no período, dado uma `tabela` de preços
    /// (provedor -> custo por resposta bem-sucedida, na unidade que o operador escolher:
    /// centavos, dólares, créditos...). Para cada provedor do relatório, multiplica os
    /// `sucessos` pelo custo unitário; quem não está na `tabela` entra com custo 0
    /// (ex.: o piso Ollama, local e grátis). Devolve sempre todos os provedores do
    /// relatório, para a conta ficar transparente (inclusive os de custo zero).
    ///
    /// Limitação honesta (código educacional): o log só guarda QUEM respondeu e QUANTO
    /// demorou — não o tamanho da resposta em tokens. Então o custo aqui é POR RESPOSTA,
    /// não por token. É uma aproximação de dependência-em-dinheiro, não a fatura exata.
    pub fn custo_por_provedor(&self, tabela: &BTreeMap<String, f64>) -> BTreeMap<String, f64> {
        self.por_provedor
            .iter()
            .map(|(nome, m)| {
                let unitario = tabela.get(nome).copied().unwrap_or(0.0);
                (nome.clone(), m.sucessos as f64 * unitario)
            })
            .collect()
    }

    /// Custo estimado total no período = soma do custo de todos os provedores.
    pub fn custo_total(&self, tabela: &BTreeMap<String, f64>) -> f64 {
        self.custo_por_provedor(tabela).values().sum()
    }

    /// Bloco de texto com o custo estimado por provedor e o total, ou `None` quando a
    /// `tabela` de preços está vazia (sem `--custo` na linha de comando, não imprime nada).
    /// Fica separado do [`Display`](std::fmt::Display) porque depende de um parâmetro externo
    /// (a tabela), e `Display` não recebe parâmetros.
    pub fn secao_custo(&self, tabela: &BTreeMap<String, f64>) -> Option<String> {
        if tabela.is_empty() {
            return None;
        }
        let mut texto = String::from("-- custo estimado (por resposta) --\n");
        for (nome, custo) in self.custo_por_provedor(tabela) {
            // Marca de onde veio o preço: da tabela, ou assumido 0 (provedor sem preço).
            let origem = if tabela.contains_key(&nome) {
                ""
            } else {
                " (sem preço → 0)"
            };
            texto.push_str(&format!("- {nome}: {custo:.2}{origem}\n"));
        }
        texto.push_str(&format!(
            "custo total estimado: {:.2}\n",
            self.custo_total(tabela)
        ));
        Some(texto)
    }

    /// Instante (epoch UTC) do último sucesso de QUALQUER provedor bom (fora do piso), ou
    /// `None` se nenhum provedor bom respondeu no período (ou só em linhas sem timestamp).
    /// É o marco a partir do qual medimos "há quanto tempo o robô está sem provedor bom".
    pub fn ultimo_sucesso_fora_do_piso_epoch(&self) -> Option<u64> {
        self.por_provedor
            .iter()
            .filter(|(nome, _)| !eh_piso(nome))
            .filter_map(|(_, m)| m.ultimo_sucesso_epoch)
            .max()
    }

    /// Há quantos SEGUNDOS o último provedor bom respondeu, relativo a `agora_epoch`.
    /// `None` se nunca houve resposta boa datada no período. Saturante: se o relógio estiver
    /// atrás do carimbo (ex.: linha do "futuro"), devolve 0 em vez de estourar.
    ///
    /// Este é o sinal-chave: enquanto a cadeia de cima estiver caindo, esse número CRESCE em
    /// tempo real — mede há quanto tempo dependemos SÓ do piso, mesmo com pouquíssimo tráfego.
    pub fn segundos_desde_ultimo_sucesso_fora_do_piso(&self, agora_epoch: u64) -> Option<u64> {
        self.ultimo_sucesso_fora_do_piso_epoch()
            .map(|marco| agora_epoch.saturating_sub(marco))
    }

    /// Bloco de texto do "frescor": por provedor, há quanto tempo respondeu pela última vez,
    /// e a linha-chave "sem provedor bom há X". Depende do `agora_epoch` (relógio real), por
    /// isso fica fora do [`Display`](std::fmt::Display) — igual a [`Relatorio::secao_custo`].
    ///
    /// Devolve `None` quando nenhum provedor tem sucesso datado (nada útil a dizer sobre
    /// frescor). Só leitura de dados já agregados — não dispara provedor nenhum.
    pub fn secao_frescor(&self, agora_epoch: u64) -> Option<String> {
        use crate::duracao::descrever_aproximada;

        // Junta quem tem pelo menos um sucesso com carimbo; sem ninguém, não há o que mostrar.
        let com_data: Vec<(&String, u64)> = self
            .por_provedor
            .iter()
            .filter_map(|(nome, m)| m.ultimo_sucesso_epoch.map(|e| (nome, e)))
            .collect();
        if com_data.is_empty() {
            return None;
        }

        let mut texto = String::from("-- frescor (último sucesso por provedor) --\n");
        for (nome, epoch) in &com_data {
            // `saturating_sub` protege contra carimbo à frente do relógio (não estoura).
            let idade = descrever_aproximada(agora_epoch.saturating_sub(*epoch));
            texto.push_str(&format!(
                "- {nome}: há {idade} ({})\n",
                crate::telemetria::formatar_data_utc(*epoch)
            ));
        }

        // A linha-chave de dependência em tempo de parede.
        match self.segundos_desde_ultimo_sucesso_fora_do_piso(agora_epoch) {
            Some(seg) => texto.push_str(&format!(
                "sem provedor bom há {}\n",
                descrever_aproximada(seg)
            )),
            None => texto.push_str("nenhum provedor bom respondeu no período (só o piso)\n"),
        }
        Some(texto)
    }

    /// Serializa o relatório inteiro como um [`Valor`](crate::json::Valor) JSON — a MESMA
    /// informação que o [`Display`](std::fmt::Display) mostra ao humano, mas legível por
    /// máquina (dashboard, alerta externo, outro programa que consome o log).
    ///
    /// A `tabela` de preços é opcional, igual ao relatório de texto: quando vazia, o bloco
    /// `custo` simplesmente não aparece (espelha o comportamento de rodar sem `--custo`).
    ///
    /// Função PURA: monta um `Valor` a partir do que já está no relatório — não lê disco,
    /// não dispara provedor nenhum (portanto jamais toca o Claude). Latências que não
    /// existem (provedor que nunca respondeu) viram `null`, não `0` — `0ms` seria mentira.
    pub fn para_json(&self, tabela: &BTreeMap<String, f64>) -> crate::json::Valor {
        use crate::json::Valor;

        // Option<u128> -> Valor: número quando há amostra, `null` quando o provedor nunca
        // respondeu (distingue "0ms" de "sem dado" — honestidade na telemetria).
        let latencia_ou_nulo = |valor: Option<u128>| match valor {
            Some(ms) => Valor::Numero(ms as f64),
            None => Valor::Nulo,
        };
        // Epoch -> Valor: número quando há, `null` quando o provedor nunca respondeu datado.
        let epoch_ou_nulo = |valor: Option<u64>| match valor {
            Some(e) => Valor::Numero(e as f64),
            None => Valor::Nulo,
        };
        // Mapa de categoria->contagem -> objeto JSON com chaves estáveis (snake_case). Objeto
        // vazio quando não houve falha — igual ao texto, que omite a linha nesse caso.
        let objeto_categorias = |mapa: &BTreeMap<CategoriaFalha, u64>| {
            Valor::Objeto(
                mapa.iter()
                    .map(|(cat, n)| (cat.chave().to_string(), Valor::Numero(*n as f64)))
                    .collect(),
            )
        };

        // Um objeto por provedor. Iteramos o BTreeMap (já ordenado por nome) → saída
        // determinística, boa para diff e para testes.
        let mut provedores: Vec<(String, Valor)> = Vec::new();
        for (nome, metricas) in &self.por_provedor {
            let objeto_provedor = Valor::Objeto(vec![
                ("sucessos".into(), Valor::Numero(metricas.sucessos as f64)),
                ("falhas".into(), Valor::Numero(metricas.falhas as f64)),
                ("pulos".into(), Valor::Numero(metricas.pulos as f64)),
                (
                    "problemas_config".into(),
                    Valor::Numero(metricas.problemas_config as f64),
                ),
                (
                    "disjuntor_pulos".into(),
                    Valor::Numero(metricas.disjuntor_pulos as f64),
                ),
                (
                    "retentativas".into(),
                    Valor::Numero(metricas.retentativas as f64),
                ),
                (
                    "sombra_pularia_ok".into(),
                    Valor::Numero(metricas.sombra_pularia_ok as f64),
                ),
                (
                    "sombra_economia_ms".into(),
                    Valor::Numero(metricas.sombra_economia_ms as f64),
                ),
                (
                    "sombra_falsos_positivos".into(),
                    Valor::Numero(metricas.sombra_falsos_positivos as f64),
                ),
                (
                    "latencia_media_ms".into(),
                    latencia_ou_nulo(metricas.latencia_media_ms()),
                ),
                (
                    "latencia_p50_ms".into(),
                    latencia_ou_nulo(metricas.latencia_percentil(50)),
                ),
                (
                    "latencia_p95_ms".into(),
                    latencia_ou_nulo(metricas.latencia_percentil(95)),
                ),
                (
                    "latencia_maxima_ms".into(),
                    latencia_ou_nulo(metricas.latencia_maxima_ms()),
                ),
                (
                    "ultimo_sucesso_epoch".into(),
                    epoch_ou_nulo(metricas.ultimo_sucesso_epoch),
                ),
                (
                    "falhas_por_categoria".into(),
                    objeto_categorias(&metricas.falhas_por_categoria),
                ),
            ]);
            provedores.push((nome.clone(), objeto_provedor));
        }

        // Campos de topo em ordem LÓGICA (não alfabética) — assim o JSON também fica legível
        // para um humano que der uma olhada, espelhando a ordem do relatório de texto.
        let mut campos: Vec<(String, Valor)> = vec![
            (
                "total_roteamentos".into(),
                Valor::Numero(self.total_roteamentos() as f64),
            ),
            (
                "caiu_no_piso".into(),
                Valor::Numero(self.sucessos_no_piso() as f64),
            ),
            (
                "percentual_no_piso".into(),
                Valor::Numero(self.percentual_no_piso()),
            ),
            (
                "sequencia_atual_no_piso".into(),
                Valor::Numero(self.sequencia_atual_no_piso as f64),
            ),
            (
                "maior_sequencia_no_piso".into(),
                Valor::Numero(self.maior_sequencia_no_piso as f64),
            ),
            // Marco absoluto do último provedor bom; a máquina consumidora calcula a idade
            // sozinha ("agora - este epoch"), então aqui não precisamos do relógio.
            (
                "ultimo_sucesso_fora_do_piso_epoch".into(),
                epoch_ou_nulo(self.ultimo_sucesso_fora_do_piso_epoch()),
            ),
            (
                "pulos_disjuntor".into(),
                Valor::Numero(self.total_pulos_disjuntor() as f64),
            ),
            (
                "retentativas".into(),
                Valor::Numero(self.total_retentativas() as f64),
            ),
            (
                "sombra_pularia_ok".into(),
                Valor::Numero(self.total_sombra_pularia_ok() as f64),
            ),
            (
                "sombra_economia_ms".into(),
                Valor::Numero(self.total_sombra_economia_ms() as f64),
            ),
            (
                "sombra_falsos_positivos".into(),
                Valor::Numero(self.total_sombra_falsos_positivos() as f64),
            ),
            (
                "falhas_por_categoria".into(),
                objeto_categorias(&self.falhas_por_categoria_total()),
            ),
            (
                "linhas_ignoradas".into(),
                Valor::Numero(self.linhas_ignoradas as f64),
            ),
            ("provedores".into(), Valor::Objeto(provedores)),
        ];

        // Bloco de custo só quando há tabela de preços (mesma regra do relatório de texto).
        if !tabela.is_empty() {
            let por_provedor_custo: Vec<(String, Valor)> = self
                .custo_por_provedor(tabela)
                .into_iter()
                .map(|(nome, custo)| (nome, Valor::Numero(custo)))
                .collect();
            let custo = Valor::Objeto(vec![
                ("por_provedor".into(), Valor::Objeto(por_provedor_custo)),
                ("total".into(), Valor::Numero(self.custo_total(tabela))),
            ]);
            campos.push(("custo".into(), custo));
        }

        Valor::Objeto(campos)
    }

    /// Atualiza as sequências de "caiu no piso" a cada `[ok]`, na ordem cronológica do log.
    /// Cada resposta de provedor bom zera a sequência atual; cada Ollama soma +1.
    fn registrar_sequencia(&mut self, nome: &str) {
        if eh_piso(nome) {
            self.sequencia_atual_no_piso += 1;
            if self.sequencia_atual_no_piso > self.maior_sequencia_no_piso {
                self.maior_sequencia_no_piso = self.sequencia_atual_no_piso;
            }
        } else {
            self.sequencia_atual_no_piso = 0;
        }
    }
}

/// Um provedor é o "piso" se o nome contém "ollama" (case-insensitive). Único ponto de decisão.
fn eh_piso(nome: &str) -> bool {
    nome.to_lowercase().contains("ollama")
}

/// Lê o arquivo de log e agrega tudo. Devolve `Err` se não conseguir ler (sem erro silencioso).
pub fn agregar_de_arquivo(caminho: &str) -> Result<Relatorio, std::io::Error> {
    let conteudo = std::fs::read_to_string(caminho)?;
    Ok(agregar(&conteudo))
}

/// Igual a [`agregar_de_arquivo`], mas só conta o que aconteceu nos últimos
/// `janela_segundos` antes de `agora_epoch` (ex.: 24h). Linhas sem timestamp ou fora
/// da janela são simplesmente ignoradas na soma (não contam como ruído).
pub fn agregar_janela_de_arquivo(
    caminho: &str,
    agora_epoch: u64,
    janela_segundos: u64,
) -> Result<Relatorio, std::io::Error> {
    let conteudo = std::fs::read_to_string(caminho)?;
    Ok(agregar_janela(&conteudo, agora_epoch, janela_segundos))
}

/// Agrega o conteúdo bruto do log (várias linhas) em um [`Relatorio`] — tudo, sem recorte.
///
/// Função pura: recebe o texto inteiro, devolve as contagens. Toda a lógica de parsing
/// é testável sem tocar em disco.
pub fn agregar(conteudo: &str) -> Relatorio {
    agregar_interno(conteudo, None)
}

/// Agrega só as linhas dentro da janela `[agora_epoch - janela_segundos, agora_epoch]`.
///
/// Útil para responder "nas últimas 24h, de quem o robô dependeu?" sem o peso do histórico
/// inteiro. Linhas anteriores à janela (ou sem timestamp legível) ficam de fora da soma.
pub fn agregar_janela(conteudo: &str, agora_epoch: u64, janela_segundos: u64) -> Relatorio {
    let inicio = agora_epoch.saturating_sub(janela_segundos);
    agregar_interno(conteudo, Some(inicio..=agora_epoch))
}

/// Núcleo compartilhado: percorre as linhas em ordem e soma os eventos. Se `janela` for
/// `Some(faixa)`, só conta linhas cujo timestamp está dentro da faixa (epoch UTC).
fn agregar_interno(conteudo: &str, janela: Option<std::ops::RangeInclusive<u64>>) -> Relatorio {
    let mut relatorio = Relatorio::default();
    for linha in conteudo.lines() {
        let (carimbo, corpo) = match separar_linha(linha) {
            Some(par) => par,
            None => {
                if !linha.trim().is_empty() {
                    relatorio.linhas_ignoradas += 1;
                }
                continue;
            }
        };
        // Recorte por janela: sem timestamp legível ou fora da faixa -> não entra na soma.
        if let Some(faixa) = &janela {
            match carimbo {
                Some(instante) if faixa.contains(&instante) => {}
                _ => continue,
            }
        }
        match classificar(corpo) {
            Some(evento) => {
                // A sequência de piso só faz sentido para roteamentos concluídos (`[ok]`).
                if let Evento::Sucesso { nome, .. } = &evento {
                    relatorio.registrar_sequencia(nome);
                }
                // Passamos o carimbo para o sucesso registrar QUANDO respondeu (frescor).
                evento.aplicar(carimbo, &mut relatorio.por_provedor);
            }
            None => relatorio.linhas_ignoradas += 1,
        }
    }
    relatorio
}

/// Um evento já interpretado de uma linha do log, pronto para somar no mapa.
enum Evento {
    Sucesso {
        nome: String,
        latencia_ms: u128,
    },
    Falha {
        nome: String,
        categoria: CategoriaFalha,
    },
    Pulo {
        nome: String,
    },
    ProblemaConfig {
        nome: String,
    },
    DisjuntorPulo {
        nome: String,
    },
    Retentativa {
        nome: String,
    },
    /// Modo sombra, previsão CERTA: pularia e o provedor de fato falhou (com a economia estimada).
    SombraPulariaOk {
        nome: String,
        economia_ms: u128,
    },
    /// Modo sombra, FALSO POSITIVO: pularia mas o provedor respondeu.
    SombraFalsoPositivo {
        nome: String,
    },
}

impl Evento {
    /// Soma este evento nas métricas do provedor correspondente (cria a entrada se faltar).
    ///
    /// `carimbo` é o instante (epoch UTC) da linha, ou `None` quando o timestamp era ilegível;
    /// só o sucesso o usa, para guardar QUANDO o provedor respondeu pela última vez (frescor).
    fn aplicar(self, carimbo: Option<u64>, por_provedor: &mut BTreeMap<String, MetricasProvedor>) {
        match self {
            Evento::Sucesso { nome, latencia_ms } => {
                let m = por_provedor.entry(nome).or_default();
                m.sucessos += 1;
                m.latencias_ms.push(latencia_ms);
                // Fica com o instante MAIS RECENTE visto (o log é cronológico, mas o `max`
                // é robusto a linhas fora de ordem). `None` de carimbo ilegível não sobrescreve.
                m.ultimo_sucesso_epoch = m.ultimo_sucesso_epoch.max(carimbo);
            }
            Evento::Falha { nome, categoria } => {
                let m = por_provedor.entry(nome).or_default();
                m.falhas += 1;
                *m.falhas_por_categoria.entry(categoria).or_insert(0) += 1;
            }
            Evento::Pulo { nome } => por_provedor.entry(nome).or_default().pulos += 1,
            Evento::ProblemaConfig { nome } => {
                por_provedor.entry(nome).or_default().problemas_config += 1
            }
            Evento::DisjuntorPulo { nome } => {
                por_provedor.entry(nome).or_default().disjuntor_pulos += 1
            }
            Evento::Retentativa { nome } => por_provedor.entry(nome).or_default().retentativas += 1,
            Evento::SombraPulariaOk { nome, economia_ms } => {
                let m = por_provedor.entry(nome).or_default();
                m.sombra_pularia_ok += 1;
                m.sombra_economia_ms += economia_ms;
            }
            Evento::SombraFalsoPositivo { nome } => {
                por_provedor
                    .entry(nome)
                    .or_default()
                    .sombra_falsos_positivos += 1
            }
        }
    }
}

/// Marcador que separa o carimbo de data do corpo da mensagem nas linhas de telemetria.
/// Formato gravado por [`telemetria::registrar_em`](crate::telemetria::registrar_em):
/// `YYYY-MM-DD HH:MM:SS UTC [roteador] <corpo>`.
const MARCADOR: &str = " [roteador] ";

/// Separa a linha em (instante, corpo): o carimbo vira epoch UTC (ou `None` se ilegível)
/// e o corpo é tudo após `[roteador] `. Devolve `None` só quando o marcador nem existe.
fn separar_linha(linha: &str) -> Option<(Option<u64>, &str)> {
    let (data, corpo) = linha.split_once(MARCADOR)?;
    Some((crate::telemetria::epoch_de_data_utc(data), corpo))
}

/// Interpreta o corpo de uma linha em um [`Evento`], ou `None` se for formato desconhecido.
///
/// Os corpos possíveis (ver `lib::rotear` e `telemetria`):
/// - `[ok] respondido por '<nome>' em <N>ms`
/// - `[falha] <nome>: <motivo> (após <N>ms) — caindo pro próximo`
/// - `[pula] <nome>: <motivo>`
/// - `[disjuntor] <nome>: disjuntor aberto (...) — pulando`
/// - `[retentativa] <nome>: <falha> — retentando (X de Y) após Zms`
/// - `[disjuntor-sombra] pularia '<nome>' ... teria economizado ~<N>ms — ele falhou como previsto`
/// - `[disjuntor-sombra] PULARIA '<nome>' ... mas ele RESPONDEU em <N>ms — FALSO POSITIVO`
/// - `<nome>: na ordem mas sem configuração` / `<nome>: tipo '...' desconhecido`
fn classificar(corpo: &str) -> Option<Evento> {
    if let Some(resto) = corpo.strip_prefix("[ok] respondido por '") {
        let nome = resto.split('\'').next()?.to_string();
        let latencia_ms = extrair_latencia_ms(resto).unwrap_or(0);
        return Some(Evento::Sucesso { nome, latencia_ms });
    }
    if let Some(resto) = corpo.strip_prefix("[falha] ") {
        return Some(Evento::Falha {
            nome: nome_antes_dos_dois_pontos(resto)?,
            categoria: categorizar_falha(resto),
        });
    }
    if let Some(resto) = corpo.strip_prefix("[pula] ") {
        return Some(Evento::Pulo {
            nome: nome_antes_dos_dois_pontos(resto)?,
        });
    }
    // Pulo por disjuntor aberto: `[disjuntor] <nome>: disjuntor aberto (...) — pulando`.
    // Só o pulo (circuito ABERTO) interessa como métrica de economia; fecha/reabre não são logados.
    if let Some(resto) = corpo.strip_prefix("[disjuntor] ") {
        return Some(Evento::DisjuntorPulo {
            nome: nome_antes_dos_dois_pontos(resto)?,
        });
    }
    // Retentativa transitória: `[retentativa] <nome>: <falha> — retentando (X de Y) após Zms`.
    if let Some(resto) = corpo.strip_prefix("[retentativa] ") {
        return Some(Evento::Retentativa {
            nome: nome_antes_dos_dois_pontos(resto)?,
        });
    }
    // Modo SOMBRA do disjuntor (dry-run). Duas formas, distinguidas pelo verbo:
    // - `pularia '<nome>' ... teria economizado ~<ms>ms — ele falhou como previsto` (previsão certa)
    // - `PULARIA '<nome>' ... mas ele RESPONDEU em <ms>ms — FALSO POSITIVO` (falso positivo)
    if let Some(resto) = corpo.strip_prefix("[disjuntor-sombra] ") {
        if let Some(depois) = resto.strip_prefix("pularia '") {
            return Some(Evento::SombraPulariaOk {
                nome: nome_entre_aspas(depois)?,
                // A economia estimada é a latência que o provedor morto gastou.
                economia_ms: extrair_latencia_ms(depois).unwrap_or(0),
            });
        }
        if let Some(depois) = resto.strip_prefix("PULARIA '") {
            return Some(Evento::SombraFalsoPositivo {
                nome: nome_entre_aspas(depois)?,
            });
        }
        // Prefixo de sombra mas forma desconhecida: não inventa evento (cai em ignoradas).
        return None;
    }
    // Problemas de config saem sem prefixo entre colchetes, mas com o padrão `<nome>: ...`.
    if corpo.contains("na ordem mas sem configuração") || corpo.contains("desconhecido") {
        return Some(Evento::ProblemaConfig {
            nome: nome_antes_dos_dois_pontos(corpo)?,
        });
    }
    None
}

/// Pega o nome do provedor antes do primeiro `:` (ex.: `claude: 401 ...` -> `claude`).
/// `None` se não houver `:` ou o nome ficar vazio.
fn nome_antes_dos_dois_pontos(corpo: &str) -> Option<String> {
    let nome = corpo.split(':').next()?.trim();
    if nome.is_empty() {
        None
    } else {
        Some(nome.to_string())
    }
}

/// Deduz a [`CategoriaFalha`] do corpo de um `[falha] <nome>: <motivo> (após ...)`.
///
/// Isola o `<motivo>` (tudo após o PRIMEIRO `:`, que separa o nome do provedor) e o classifica.
/// O `<motivo>` é o [`Display`](std::fmt::Display) de uma
/// [`FalhaProvedor`](crate::erro::FalhaProvedor): `http <status>: ...`, `rede: ...`,
/// `processo: ...`, `indisponível: ...`, `resposta vazia`/`resposta inválida: ...`.
fn categorizar_falha(corpo_apos_falha: &str) -> CategoriaFalha {
    match corpo_apos_falha.split_once(':') {
        Some((_nome, motivo)) => categorizar_motivo(motivo.trim()),
        // Sem `:` não há motivo estruturado (formato inesperado) — cai em "outra".
        None => CategoriaFalha::Outra,
    }
}

/// Classifica o texto do motivo (já sem o nome do provedor) em uma [`CategoriaFalha`].
/// Espelha, do lado da leitura, a mesma taxonomia que [`crate::erro::FalhaProvedor`] grava.
fn categorizar_motivo(motivo: &str) -> CategoriaFalha {
    if let Some(resto) = motivo.strip_prefix("http ") {
        // Os dígitos logo após "http " são o status; sem dígitos, cai em "outra" (status 0).
        let status: u16 = resto
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        return match status {
            401 | 403 => CategoriaFalha::Autenticacao,
            429 => CategoriaFalha::LimiteTaxa,
            408 => CategoriaFalha::ServidorInstavel,
            400 | 404 | 413 | 422 => CategoriaFalha::RequisicaoInvalida,
            s if (500..=599).contains(&s) => CategoriaFalha::ServidorInstavel,
            _ => CategoriaFalha::Outra,
        };
    }
    if motivo.starts_with("rede") {
        return CategoriaFalha::Rede;
    }
    if motivo.starts_with("processo") {
        return CategoriaFalha::Processo;
    }
    if motivo.starts_with("indisponível") {
        return CategoriaFalha::Configuracao;
    }
    if motivo.starts_with("resposta vazia") || motivo.starts_with("resposta inválida") {
        return CategoriaFalha::RespostaRuim;
    }
    CategoriaFalha::Outra
}

/// Pega o nome até a próxima aspa simples, dado um texto que COMEÇA logo após a aspa de
/// abertura (ex.: `topo' (circuito ...` -> `topo`). `None` se não houver aspa de fecho ou
/// o nome ficar vazio. Usado nas linhas do modo sombra, onde o nome vem entre aspas.
fn nome_entre_aspas(depois_da_aspa: &str) -> Option<String> {
    // `split_once` EXIGE a aspa de fecho; sem ela, devolve None (não engole o resto da linha).
    let (nome, _) = depois_da_aspa.split_once('\'')?;
    let nome = nome.trim();
    if nome.is_empty() {
        None
    } else {
        Some(nome.to_string())
    }
}

/// Acha o número de milissegundos numa frase como `... em 33ms` ou `... (após 1200ms)`.
/// Procura o sufixo `ms` e anda para trás juntando os dígitos coladinhos antes dele.
fn extrair_latencia_ms(texto: &str) -> Option<u128> {
    let posicao_ms = texto.find("ms")?;
    let antes = &texto[..posicao_ms];
    let digitos: String = antes
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digitos.parse().ok()
}

/// Relatório em texto legível, pronto para imprimir no terminal.
///
/// Mostra, por provedor, sucessos/falhas/pulos e latência média; e fecha com a linha
/// que mais importa: quantas vezes (e em que %) o robô caiu no piso (Ollama).
impl std::fmt::Display for Relatorio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "== Métricas do roteador de provedores ==")?;
        let total = self.total_roteamentos();
        if self.por_provedor.is_empty() {
            writeln!(f, "(sem dados no log ainda)")?;
            return Ok(());
        }
        for (nome, m) in &self.por_provedor {
            // Latência: sempre a média; com 2+ respostas, também p50/p95/máx para revelar a
            // cauda que a média esconde. Com 1 só resposta, os percentis seriam iguais à
            // média (ruído), então mostramos só a média.
            let latencia = match m.latencia_media_ms() {
                None => "—".to_string(),
                Some(media) if m.sucessos >= 2 => {
                    let p50 = m.latencia_percentil(50).unwrap_or(media);
                    let p95 = m.latencia_percentil(95).unwrap_or(media);
                    let maxima = m.latencia_maxima_ms().unwrap_or(media);
                    format!("{media}ms média (p50 {p50} / p95 {p95} / máx {maxima})")
                }
                Some(media) => format!("{media}ms média"),
            };
            // O pulo por disjuntor só aparece quando houve algum — mantém a linha enxuta
            // no caso comum (disjuntor desligado), sem poluir com "0 disjuntor" em todo lugar.
            let disjuntor = if m.disjuntor_pulos > 0 {
                format!(", {} disjuntor", m.disjuntor_pulos)
            } else {
                String::new()
            };
            // Retentativas idem: só aparecem quando houve alguma (linha enxuta no caso comum).
            let retentativas = if m.retentativas > 0 {
                format!(", {} retentativa", m.retentativas)
            } else {
                String::new()
            };
            writeln!(
                f,
                "- {nome}: {} ok, {} falha, {} pulo, {} cfg{disjuntor}{retentativas} | {latencia}",
                m.sucessos, m.falhas, m.pulos, m.problemas_config
            )?;
            // Abre o "por quê" das falhas deste provedor (auth/timeout/rede...) — o sinal mais
            // acionável. Só aparece quando houve falha, para não poluir a linha no caso limpo.
            if m.falhas > 0 {
                writeln!(f, "    ↳ falhas por motivo: {}", m.resumo_falhas())?;
            }
        }
        writeln!(f, "total de roteamentos: {total}")?;
        // Agregado das falhas por motivo (todos os provedores) — "de que a cadeia morreu no
        // período". Só aparece se houve alguma falha.
        let falhas_agregadas = self.falhas_por_categoria_total();
        if !falhas_agregadas.is_empty() {
            let resumo = falhas_agregadas
                .iter()
                .map(|(cat, n)| format!("{} {n}", cat.rotulo()))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(f, "falhas por motivo (total): {resumo}")?;
        }
        // Economia do disjuntor: só reporta se ele chegou a pular alguém no período.
        let pulos_disjuntor = self.total_pulos_disjuntor();
        if pulos_disjuntor > 0 {
            writeln!(
                f,
                "provedores pulados por disjuntor (latência de morto evitada): {pulos_disjuntor}"
            )?;
        }
        // Retentativas: só reporta se houve alguma (blips passageiros absorvidos no período).
        let retentativas = self.total_retentativas();
        if retentativas > 0 {
            writeln!(
                f,
                "retentativas transitórias (blips absorvidos): {retentativas}"
            )?;
        }
        // Modo sombra do disjuntor: só reporta se houve atividade (disjuntor rodando em sombra).
        // É a projeção "e se eu ligasse o disjuntor?": quantos pulos acertaria, quanto pouparia,
        // e — o freio — quantos falsos positivos (pularia um provedor que respondeu).
        let sombra_ok = self.total_sombra_pularia_ok();
        let sombra_fp = self.total_sombra_falsos_positivos();
        if sombra_ok > 0 || sombra_fp > 0 {
            let economia = self.total_sombra_economia_ms();
            writeln!(
                f,
                "disjuntor em sombra: pularia {sombra_ok} certo(s) (~{economia}ms poupados), {sombra_fp} falso(s) positivo(s)"
            )?;
            if sombra_fp == 0 {
                writeln!(f, "  → sem falsos positivos: candidato a ligar o disjuntor")?;
            } else {
                writeln!(
                    f,
                    "  → {sombra_fp} falso(s) positivo(s): NÃO ligar ainda / subir limiar/cooldown"
                )?;
            }
        }
        let piso = self.sucessos_no_piso();
        let pct = self.percentual_no_piso();
        writeln!(f, "caiu no piso (Ollama): {piso} de {total} ({pct:.1}%)")?;
        // Sequências de piso: alarme de dependência AGORA (atual) e pior momento (máxima).
        writeln!(
            f,
            "sequência no piso: {} agora (máx. {})",
            self.sequencia_atual_no_piso, self.maior_sequencia_no_piso
        )?;
        if self.linhas_ignoradas > 0 {
            writeln!(f, "(linhas ignoradas: {})", self.linhas_ignoradas)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn separa_carimbo_e_corpo_depois_do_marcador() {
        let linha = "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama' em 33ms";
        let (carimbo, corpo) = separar_linha(linha).unwrap();
        assert_eq!(corpo, "[ok] respondido por 'ollama' em 33ms");
        // O carimbo vira epoch (mesmo instante que a telemetria gravaria).
        assert_eq!(
            carimbo,
            crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC")
        );
        // Sem o marcador, nem dá pra separar.
        assert_eq!(separar_linha("linha sem marcador"), None);
        // Com marcador mas data corrompida: separa o corpo, mas o carimbo fica None.
        let (sem_data, corpo2) = separar_linha("lixo [roteador] [pula] groq: x").unwrap();
        assert_eq!(sem_data, None);
        assert_eq!(corpo2, "[pula] groq: x");
    }

    #[test]
    fn extrai_latencia_em_varios_formatos() {
        assert_eq!(extrair_latencia_ms("em 33ms"), Some(33));
        assert_eq!(extrair_latencia_ms("(após 1200ms) — caindo"), Some(1200));
        assert_eq!(extrair_latencia_ms("sem numero ms"), None);
        assert_eq!(extrair_latencia_ms("sem ms aqui não tem nada"), None);
    }

    #[test]
    fn classifica_sucesso_com_nome_e_latencia() {
        match classificar("[ok] respondido por 'claude' em 850ms") {
            Some(Evento::Sucesso { nome, latencia_ms }) => {
                assert_eq!(nome, "claude");
                assert_eq!(latencia_ms, 850);
            }
            outro => panic!("esperava Sucesso, veio outro: {:?}", outro.is_some()),
        }
    }

    #[test]
    fn classifica_falha_pula_e_config() {
        assert!(matches!(
            classificar("[falha] claude: 401 não autorizado (após 90ms) — caindo pro próximo"),
            Some(Evento::Falha { nome, .. }) if nome == "claude"
        ));
        assert!(matches!(
            classificar("[pula] groq: desabilitado ou sem chave"),
            Some(Evento::Pulo { nome }) if nome == "groq"
        ));
        assert!(matches!(
            classificar("gemini: na ordem mas sem configuração"),
            Some(Evento::ProblemaConfig { nome }) if nome == "gemini"
        ));
        assert!(classificar("linha aleatória qualquer").is_none());
        // A linha de retentativa deve ter um lar (não cair em "ignoradas").
        assert!(matches!(
            classificar("[retentativa] groq: rede: piscou — retentando (1 de 2) após 250ms"),
            Some(Evento::Retentativa { nome }) if nome == "groq"
        ));
    }

    #[test]
    fn categoriza_motivo_de_cada_tipo_de_falha() {
        // HTTP mapeado por status (usa o Display real da FalhaProvedor: "http <status>: ...").
        assert_eq!(
            categorizar_motivo("http 401: não autorizado"),
            CategoriaFalha::Autenticacao
        );
        assert_eq!(
            categorizar_motivo("http 403: proibido"),
            CategoriaFalha::Autenticacao
        );
        assert_eq!(
            categorizar_motivo("http 429: rate limit"),
            CategoriaFalha::LimiteTaxa
        );
        assert_eq!(
            categorizar_motivo("http 503: indisponível"),
            CategoriaFalha::ServidorInstavel
        );
        assert_eq!(
            categorizar_motivo("http 408: timeout"),
            CategoriaFalha::ServidorInstavel
        );
        assert_eq!(
            categorizar_motivo("http 400: pedido ruim"),
            CategoriaFalha::RequisicaoInvalida
        );
        // Não-HTTP: rede, processo, config e resposta ruim.
        assert_eq!(
            categorizar_motivo("rede: conexão recusada"),
            CategoriaFalha::Rede
        );
        assert_eq!(
            categorizar_motivo("processo: código de saída 1"),
            CategoriaFalha::Processo
        );
        assert_eq!(
            categorizar_motivo("indisponível: sem chave"),
            CategoriaFalha::Configuracao
        );
        assert_eq!(
            categorizar_motivo("resposta vazia"),
            CategoriaFalha::RespostaRuim
        );
        assert_eq!(
            categorizar_motivo("resposta inválida: json sem campo"),
            CategoriaFalha::RespostaRuim
        );
        // Status esquisito e texto desconhecido caem em "outra" (nunca deveriam dominar).
        assert_eq!(
            categorizar_motivo("http 418: bule de chá"),
            CategoriaFalha::Outra
        );
        assert_eq!(
            categorizar_motivo("algo que não conheço"),
            CategoriaFalha::Outra
        );
    }

    #[test]
    fn falha_leva_a_categoria_no_evento_e_no_relatorio() {
        // A linha `[falha]` completa deve virar Evento::Falha COM a categoria certa.
        match classificar("[falha] gemini: http 429: cota (após 120ms) — caindo pro próximo") {
            Some(Evento::Falha { nome, categoria }) => {
                assert_eq!(nome, "gemini");
                assert_eq!(categoria, CategoriaFalha::LimiteTaxa);
            }
            _ => panic!("esperava Evento::Falha com categoria"),
        }

        // Agregando um log inteiro: o Claude falha por auth 2x e por timeout 1x; o resumo
        // por provedor e o agregado total devem refletir o "por quê".
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [falha] claude: http 401: token caiu (após 90ms) — caindo pro próximo
2026-06-30 12:00:01 UTC [roteador] [falha] claude: http 401: token caiu (após 90ms) — caindo pro próximo
2026-06-30 12:00:02 UTC [roteador] [falha] claude: http 503: instável (após 90ms) — caindo pro próximo
2026-06-30 12:00:03 UTC [roteador] [ok] respondido por 'ollama_local' em 33ms
";
        let r = agregar(log);
        let claude = &r.por_provedor["claude"];
        assert_eq!(claude.falhas, 3);
        assert_eq!(
            claude.falhas_por_categoria[&CategoriaFalha::Autenticacao],
            2
        );
        assert_eq!(
            claude.falhas_por_categoria[&CategoriaFalha::ServidorInstavel],
            1
        );
        // A soma das categorias tem que bater com o total de falhas (invariante).
        let soma: u64 = claude.falhas_por_categoria.values().sum();
        assert_eq!(soma, claude.falhas);
        // Resumo em texto, na ordem do enum (Autenticacao vem antes de ServidorInstavel).
        assert_eq!(claude.resumo_falhas(), "auth 2, timeout/5xx 1");
        // Agregado total (só o claude falhou aqui).
        let total = r.falhas_por_categoria_total();
        assert_eq!(total[&CategoriaFalha::Autenticacao], 2);
        assert_eq!(total[&CategoriaFalha::ServidorInstavel], 1);
    }

    #[test]
    fn json_traz_falhas_por_categoria() {
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [falha] claude: http 401: x (após 90ms) — caindo pro próximo
2026-06-30 12:00:01 UTC [roteador] [ok] respondido por 'ollama_local' em 33ms
";
        let r = agregar(log);
        let json = r.para_json(&BTreeMap::new()).para_texto();
        // Topo e por-provedor devem citar a categoria com a CHAVE estável (snake_case).
        assert!(
            json.contains("\"falhas_por_categoria\""),
            "faltou o campo no JSON: {json}"
        );
        assert!(
            json.contains("\"autenticacao\":1"),
            "faltou a contagem de auth no JSON: {json}"
        );
    }

    #[test]
    fn conta_retentativas_por_provedor_e_no_total() {
        // groq sofre 2 blips transitórios (retentativas), depois responde. A retentativa não
        // é sucesso nem falha: é uma dimensão própria, e não deve virar "linha ignorada".
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [retentativa] groq: rede: piscou — retentando (1 de 2) após 250ms
2026-06-30 12:00:01 UTC [roteador] [retentativa] groq: http 503:  — retentando (2 de 2) após 500ms
2026-06-30 12:00:02 UTC [roteador] [ok] respondido por 'groq' em 900ms
";
        let r = agregar(log);
        assert_eq!(r.por_provedor["groq"].retentativas, 2);
        assert_eq!(r.por_provedor["groq"].sucessos, 1);
        assert_eq!(r.total_retentativas(), 2);
        assert_eq!(r.linhas_ignoradas, 0, "retentativa tem lar, não é ruído");
    }

    #[test]
    fn agrega_log_de_exemplo() {
        // Cenário realista: groq pulado e claude falhando algumas vezes, Ollama segurando o piso,
        // e o claude também respondendo em outras. Queremos as contagens e a média certas.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [pula] groq: desabilitado ou sem chave
2026-06-30 12:00:01 UTC [roteador] [falha] claude: 401 (após 90ms) — caindo pro próximo
2026-06-30 12:00:33 UTC [roteador] [ok] respondido por 'ollama_local' em 33000ms
2026-06-30 12:05:00 UTC [roteador] [ok] respondido por 'claude' em 800ms
2026-06-30 12:06:00 UTC [roteador] [ok] respondido por 'claude' em 1200ms
linha de ruído sem formato
";
        let r = agregar(log);

        let groq = &r.por_provedor["groq"];
        assert_eq!(groq.pulos, 1);

        let claude = &r.por_provedor["claude"];
        assert_eq!(claude.sucessos, 2);
        assert_eq!(claude.falhas, 1);
        assert_eq!(claude.latencia_media_ms(), Some(1000)); // (800 + 1200) / 2

        let ollama = &r.por_provedor["ollama_local"];
        assert_eq!(ollama.sucessos, 1);

        assert_eq!(r.total_roteamentos(), 3); // 2 claude + 1 ollama
        assert_eq!(r.sucessos_no_piso(), 1); // só o ollama_local
        assert_eq!(r.linhas_ignoradas, 1); // a linha de ruído
    }

    #[test]
    fn conta_sequencia_consecutiva_no_piso() {
        // Ordem cronológica: bom, piso, piso, piso, bom, piso, piso.
        // Maior sequência seguida = 3; a atual (no fim) = 2.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 31000ms
2026-06-30 12:03:00 UTC [roteador] [ok] respondido por 'ollama_local' em 32000ms
2026-06-30 12:04:00 UTC [roteador] [ok] respondido por 'claude' em 600ms
2026-06-30 12:05:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:06:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let r = agregar(log);
        assert_eq!(r.maior_sequencia_no_piso, 3);
        assert_eq!(r.sequencia_atual_no_piso, 2);
        assert!(format!("{r}").contains("sequência no piso: 2 agora (máx. 3)"));
    }

    #[test]
    fn janela_de_tempo_recorta_o_log() {
        // Duas linhas: uma velha (fora de 1h) e uma recente (dentro). Só a recente conta.
        let velha = crate::telemetria::epoch_de_data_utc("2026-06-30 10:00:00 UTC").unwrap();
        let recente = crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC").unwrap();
        let agora = recente; // "agora" = instante da linha recente
        let log = "\
2026-06-30 10:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        // Janela de 1h: deixa de fora a linha das 10:00.
        let r = agregar_janela(log, agora, 3_600);
        assert!(!r.por_provedor.contains_key("claude"));
        assert_eq!(r.por_provedor["ollama_local"].sucessos, 1);
        assert_eq!(r.total_roteamentos(), 1);

        // Janela larga (3h): pega as duas. Confirma que o recorte é o que muda.
        let r3h = agregar_janela(log, agora, 3 * 3_600);
        assert_eq!(r3h.total_roteamentos(), 2);
        let _ = velha; // documentado: linha velha existe, só foi recortada na janela de 1h
    }

    #[test]
    fn linha_sem_timestamp_legivel_fica_fora_da_janela() {
        // Timestamp corrompido -> não dá pra situar no tempo -> não entra na visão por janela.
        let log = "data-quebrada [roteador] [ok] respondido por 'claude' em 500ms\n";
        let r = agregar_janela(log, 2_000_000_000, 86_400);
        assert_eq!(r.total_roteamentos(), 0);
        // Mas no agregado completo (sem janela) ela conta normalmente.
        assert_eq!(agregar(log).total_roteamentos(), 1);
    }

    #[test]
    fn relatorio_vazio_nao_quebra() {
        let r = agregar("");
        assert_eq!(r.total_roteamentos(), 0);
        assert!(format!("{r}").contains("sem dados"));
    }

    #[test]
    fn display_mostra_percentual_do_piso() {
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let texto = format!("{}", agregar(log));
        assert!(texto.contains("total de roteamentos: 2"));
        assert!(texto.contains("caiu no piso (Ollama): 1 de 2 (50.0%)"));
    }

    #[test]
    fn custo_estimado_por_provedor_e_total() {
        // claude respondeu 2x, ollama 1x. Preço: claude 3.0/resposta, ollama fora da tabela (0).
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'claude' em 600ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let r = agregar(log);
        let mut tabela = BTreeMap::new();
        tabela.insert("claude".to_string(), 3.0);

        let por_provedor = r.custo_por_provedor(&tabela);
        assert_eq!(por_provedor["claude"], 6.0); // 2 respostas * 3.0
        assert_eq!(por_provedor["ollama_local"], 0.0); // sem preço = grátis (piso local)
        assert_eq!(r.custo_total(&tabela), 6.0);
    }

    #[test]
    fn secao_custo_so_aparece_com_tabela() {
        let log = "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms\n";
        let r = agregar(log);

        // Sem tabela: nada a mostrar.
        assert!(r.secao_custo(&BTreeMap::new()).is_none());

        // Com tabela: bloco com o provedor, seu custo e o total.
        let mut tabela = BTreeMap::new();
        tabela.insert("claude".to_string(), 2.5);
        let texto = r.secao_custo(&tabela).unwrap();
        assert!(texto.contains("- claude: 2.50"));
        assert!(texto.contains("custo total estimado: 2.50"));
        // Provedor sem preço aparece marcado como assumido 0 (transparência da conta).
        let log2 =
            "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms\n";
        let texto2 = agregar(log2).secao_custo(&tabela).unwrap();
        assert!(texto2.contains("ollama_local: 0.00 (sem preço → 0)"));
    }

    #[test]
    fn classifica_e_conta_pulo_por_disjuntor() {
        // A linha do disjuntor aberto deve virar um DisjuntorPulo com o nome certo...
        assert!(matches!(
            classificar("[disjuntor] claude: disjuntor aberto (3 falhas seguidas) — pulando"),
            Some(Evento::DisjuntorPulo { nome }) if nome == "claude"
        ));

        // ...e ser somada por provedor (sem contar como sucesso/falha/pulo comum nem como ruído).
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [disjuntor] claude: disjuntor aberto (3 falhas seguidas) — pulando
2026-06-30 12:00:01 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:05:00 UTC [roteador] [disjuntor] claude: disjuntor aberto (3 falhas seguidas) — pulando
2026-06-30 12:05:01 UTC [roteador] [ok] respondido por 'ollama_local' em 31000ms
";
        let r = agregar(log);
        assert_eq!(r.por_provedor["claude"].disjuntor_pulos, 2);
        assert_eq!(r.por_provedor["claude"].sucessos, 0);
        assert_eq!(r.por_provedor["claude"].falhas, 0);
        assert_eq!(r.total_pulos_disjuntor(), 2);
        assert_eq!(r.linhas_ignoradas, 0); // antes da métrica, essas 2 linhas eram "ruído"

        // O relatório mostra o pulo na linha do claude e a linha-resumo agregada.
        let texto = format!("{r}");
        assert!(texto.contains(", 2 disjuntor"));
        assert!(texto.contains("provedores pulados por disjuntor (latência de morto evitada): 2"));
    }

    #[test]
    fn classifica_linhas_do_modo_sombra() {
        // Previsão certa: pularia e o provedor falhou, com economia estimada extraída.
        match classificar("[disjuntor-sombra] pularia 'claude' (circuito aberto, 3 falhas seguidas) e teria economizado ~1200ms — ele falhou como previsto") {
            Some(Evento::SombraPulariaOk { nome, economia_ms }) => {
                assert_eq!(nome, "claude");
                assert_eq!(economia_ms, 1200);
            }
            outro => panic!("esperava SombraPulariaOk, veio {:?}", outro.is_some()),
        }
        // Falso positivo: pularia mas o provedor respondeu.
        assert!(matches!(
            classificar("[disjuntor-sombra] PULARIA 'gemini' (circuito aberto, 4 falhas seguidas) mas ele RESPONDEU em 900ms — FALSO POSITIVO (não ligar ainda / afinar limiar)"),
            Some(Evento::SombraFalsoPositivo { nome }) if nome == "gemini"
        ));
        // Forma de sombra desconhecida não vira evento (cai em ignoradas, não inventa).
        assert!(classificar("[disjuntor-sombra] algo estranho sem verbo conhecido").is_none());
    }

    #[test]
    fn agrega_atividade_do_modo_sombra() {
        // Duas previsões certas do claude (economia 1200 + 800) e um falso positivo do gemini.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [disjuntor-sombra] pularia 'claude' (circuito aberto, 3 falhas seguidas) e teria economizado ~1200ms — ele falhou como previsto
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:05:00 UTC [roteador] [disjuntor-sombra] pularia 'claude' (circuito aberto, 3 falhas seguidas) e teria economizado ~800ms — ele falhou como previsto
2026-06-30 12:05:00 UTC [roteador] [ok] respondido por 'ollama_local' em 31000ms
2026-06-30 12:10:00 UTC [roteador] [disjuntor-sombra] PULARIA 'gemini' (circuito aberto, 5 falhas seguidas) mas ele RESPONDEU em 900ms — FALSO POSITIVO (não ligar ainda / afinar limiar)
2026-06-30 12:10:00 UTC [roteador] [ok] respondido por 'gemini' em 900ms
";
        let r = agregar(log);
        assert_eq!(r.por_provedor["claude"].sombra_pularia_ok, 2);
        assert_eq!(r.por_provedor["claude"].sombra_economia_ms, 2000); // 1200 + 800
        assert_eq!(r.por_provedor["gemini"].sombra_falsos_positivos, 1);
        assert_eq!(r.total_sombra_pularia_ok(), 2);
        assert_eq!(r.total_sombra_economia_ms(), 2000);
        assert_eq!(r.total_sombra_falsos_positivos(), 1);
        // Linhas de sombra têm lar: não contam como ruído.
        assert_eq!(r.linhas_ignoradas, 0);

        // O relatório mostra o resumo da sombra e, havendo falso positivo, o freio.
        let texto = format!("{r}");
        assert!(texto.contains(
            "disjuntor em sombra: pularia 2 certo(s) (~2000ms poupados), 1 falso(s) positivo(s)"
        ));
        assert!(texto.contains("NÃO ligar ainda"));
    }

    #[test]
    fn sombra_sem_falso_positivo_sugere_ligar() {
        // Só previsões certas: o relatório sinaliza que o disjuntor é candidato a ser ligado.
        let log = "2026-06-30 12:00:00 UTC [roteador] [disjuntor-sombra] pularia 'claude' (circuito aberto, 3 falhas seguidas) e teria economizado ~1500ms — ele falhou como previsto\n";
        let texto = format!("{}", agregar(log));
        assert!(texto.contains("candidato a ligar o disjuntor"));
        assert!(!texto.contains("NÃO ligar"));
    }

    #[test]
    fn sem_sombra_o_relatorio_nao_mostra_a_linha() {
        // Caso comum (sombra desligada): nada de "sombra" na saída, para não poluir.
        let log =
            "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms\n";
        let texto = format!("{}", agregar(log));
        assert!(!texto.contains("sombra"));
    }

    #[test]
    fn sem_disjuntor_o_relatorio_nao_mostra_a_linha() {
        // Caso comum (disjuntor desligado): nada de "disjuntor" na saída, para não poluir.
        let log =
            "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms\n";
        let texto = format!("{}", agregar(log));
        assert!(!texto.contains("disjuntor"));
    }

    #[test]
    fn percentis_e_maximo_de_latencia() {
        // 20 respostas de 100ms e uma de 5000ms: a média fica "ok", mas o máx e o p95 gritam.
        let mut m = MetricasProvedor::default();
        for _ in 0..20 {
            m.sucessos += 1;
            m.latencias_ms.push(100);
        }
        m.sucessos += 1;
        m.latencias_ms.push(5000);

        // Média puxada só um pouco pela travada: (20*100 + 5000)/21 = 333ms.
        assert_eq!(m.latencia_media_ms(), Some(333));
        // p50 = mediana = 100ms (a maioria é rápida).
        assert_eq!(m.latencia_percentil(50), Some(100));
        // p95 sobre 21 amostras: ceil(0.95*21)=20 -> índice 19 -> ainda 100ms (só 1 é lenta).
        assert_eq!(m.latencia_percentil(95), Some(100));
        // O máximo é o único jeito de ver a travada de 5s.
        assert_eq!(m.latencia_maxima_ms(), Some(5000));
        // p100 == máximo; p0 == mínimo.
        assert_eq!(m.latencia_percentil(100), Some(5000));
        assert_eq!(m.latencia_percentil(0), Some(100));
    }

    #[test]
    fn percentis_de_provedor_sem_resposta_sao_none() {
        let m = MetricasProvedor::default();
        assert_eq!(m.latencia_media_ms(), None);
        assert_eq!(m.latencia_percentil(95), None);
        assert_eq!(m.latencia_maxima_ms(), None);
    }

    #[test]
    fn display_mostra_percentis_com_duas_ou_mais_respostas() {
        // claude respondeu 2x (800 e 1200ms) -> aparece p50/p95/máx; ollama 1x -> só média.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 800ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'claude' em 1200ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let texto = format!("{}", agregar(log));
        // Duas respostas: mostra a cauda (máx = 1200, o pior caso).
        assert!(texto.contains("1000ms média (p50 800 / p95 1200 / máx 1200)"));
        // Uma só resposta: sem percentis redundantes, apenas a média.
        assert!(texto.contains("30000ms média"));
        assert!(!texto.contains("30000ms média (p50"));
    }

    #[test]
    fn percentual_no_piso_calcula_a_fracao() {
        // 3 no piso de 4 roteamentos = 75%.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 31000ms
2026-06-30 12:03:00 UTC [roteador] [ok] respondido por 'ollama_local' em 32000ms
";
        let r = agregar(log);
        assert_eq!(r.percentual_no_piso(), 75.0);
        // Sem roteamento nenhum, não divide por zero: fica 0%.
        assert_eq!(agregar("").percentual_no_piso(), 0.0);
    }

    #[test]
    fn para_json_espelha_o_relatorio_e_faz_round_trip() {
        // claude respondeu 1x, ollama 2x -> 2 de 3 no piso.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 800ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 32000ms
";
        let relatorio = agregar(log);
        let tabela = BTreeMap::new(); // sem preços -> sem bloco "custo"
                                      // Serializa e re-parseia com nosso próprio parser (garante JSON válido de verdade).
        let texto = relatorio.para_json(&tabela).para_texto();
        let valor = crate::json::parsear(&texto).expect("JSON gerado deve ser válido");

        // Campos de topo batem com os métodos do relatório.
        assert_eq!(
            valor.obter("total_roteamentos").unwrap().como_numero(),
            Some(3.0)
        );
        assert_eq!(
            valor.obter("caiu_no_piso").unwrap().como_numero(),
            Some(2.0)
        );
        assert_eq!(
            valor.obter("percentual_no_piso").unwrap().como_numero(),
            Some(relatorio.percentual_no_piso())
        );

        // O provedor claude aparece com 1 sucesso e latência média 800ms.
        let provedores = valor.obter("provedores").unwrap();
        let claude = provedores.obter("claude").unwrap();
        assert_eq!(claude.obter("sucessos").unwrap().como_numero(), Some(1.0));
        assert_eq!(
            claude.obter("latencia_media_ms").unwrap().como_numero(),
            Some(800.0)
        );

        // Sem tabela de preços, o bloco de custo NÃO existe (espelha o relatório de texto).
        assert!(valor.obter("custo").is_none());
    }

    #[test]
    fn frescor_guarda_ultimo_sucesso_por_provedor() {
        // claude respondeu 2x (a segunda mais tarde), ollama 1x. O último sucesso de cada um
        // deve ser o instante da linha MAIS RECENTE dele.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 800ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:05:00 UTC [roteador] [ok] respondido por 'claude' em 900ms
";
        let r = agregar(log);
        let esperado_claude = crate::telemetria::epoch_de_data_utc("2026-06-30 12:05:00 UTC");
        let esperado_ollama = crate::telemetria::epoch_de_data_utc("2026-06-30 12:01:00 UTC");
        assert_eq!(
            r.por_provedor["claude"].ultimo_sucesso_epoch,
            esperado_claude
        );
        assert_eq!(
            r.por_provedor["ollama_local"].ultimo_sucesso_epoch,
            esperado_ollama
        );
        // O último provedor BOM (fora do piso) é o claude das 12:05.
        assert_eq!(r.ultimo_sucesso_fora_do_piso_epoch(), esperado_claude);
    }

    #[test]
    fn frescor_sem_sucesso_datado_fica_sem_marco() {
        // Só falhas/pulos: ninguém respondeu -> sem marco de frescor, seção some.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [pula] groq: sem chave
2026-06-30 12:00:01 UTC [roteador] [falha] claude: 401 (após 90ms) — caindo pro próximo
";
        let r = agregar(log);
        assert_eq!(r.ultimo_sucesso_fora_do_piso_epoch(), None);
        assert_eq!(
            r.segundos_desde_ultimo_sucesso_fora_do_piso(2_000_000_000),
            None
        );
        assert!(r.secao_frescor(2_000_000_000).is_none());
    }

    #[test]
    fn segundos_desde_ultimo_sucesso_conta_do_agora() {
        // Um sucesso bom às 12:00; "agora" 2h depois -> 7200s sem provedor bom.
        let log = "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms\n";
        let r = agregar(log);
        let marco = crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC").unwrap();
        let agora = marco + 7_200;
        assert_eq!(
            r.segundos_desde_ultimo_sucesso_fora_do_piso(agora),
            Some(7_200)
        );
        // Relógio ATRÁS do carimbo (linha do "futuro") satura em 0, não estoura.
        assert_eq!(
            r.segundos_desde_ultimo_sucesso_fora_do_piso(marco - 10),
            Some(0)
        );
    }

    #[test]
    fn secao_frescor_mostra_provedor_e_linha_chave() {
        // claude respondeu às 12:00; ollama às 12:30. "Agora" = 12:00 + 3h.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:30:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let r = agregar(log);
        let marco_claude = crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC").unwrap();
        let agora = marco_claude + 3 * 3_600; // 15:00
        let texto = r.secao_frescor(agora).unwrap();
        // O claude bom respondeu há 3h; a linha-chave mede desde o último BOM (não o piso).
        assert!(texto.contains("- claude: há 3h"));
        assert!(texto.contains("sem provedor bom há 3h"));
        // O ollama entra na lista por provedor (respondeu há 2h30min), mas NÃO conta como bom.
        assert!(texto.contains("- ollama_local: há 2h30min"));
    }

    #[test]
    fn secao_frescor_avisa_quando_so_o_piso_respondeu() {
        // Só o piso respondeu: a linha-chave deixa explícito que nunca teve provedor bom.
        let log =
            "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms\n";
        let r = agregar(log);
        let agora = crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC").unwrap() + 60;
        let texto = r.secao_frescor(agora).unwrap();
        assert!(texto.contains("- ollama_local: há 1min"));
        assert!(texto.contains("nenhum provedor bom respondeu no período (só o piso)"));
    }

    #[test]
    fn para_json_inclui_frescor() {
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 800ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let relatorio = agregar(log);
        let valor = relatorio.para_json(&BTreeMap::new());
        let marco = crate::telemetria::epoch_de_data_utc("2026-06-30 12:00:00 UTC").unwrap();
        // Topo: marco absoluto do último provedor bom.
        assert_eq!(
            valor
                .obter("ultimo_sucesso_fora_do_piso_epoch")
                .unwrap()
                .como_numero(),
            Some(marco as f64)
        );
        // Por provedor: o claude traz seu próprio epoch de último sucesso.
        let claude = valor.obter("provedores").unwrap().obter("claude").unwrap();
        assert_eq!(
            claude.obter("ultimo_sucesso_epoch").unwrap().como_numero(),
            Some(marco as f64)
        );
    }

    #[test]
    fn para_json_inclui_custo_so_quando_ha_precos() {
        let log = "2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms\n";
        let relatorio = agregar(log);

        // Latência nula vira `null`, não 0, para provedor que nunca respondeu.
        let ollama_metricas = MetricasProvedor::default();
        assert_eq!(ollama_metricas.latencia_media_ms(), None);

        let mut tabela = BTreeMap::new();
        tabela.insert("claude".to_string(), 3.0);
        let valor = relatorio.para_json(&tabela);
        let custo = valor.obter("custo").expect("com preço, bloco custo existe");
        assert_eq!(custo.obter("total").unwrap().como_numero(), Some(3.0));
        assert_eq!(
            custo
                .obter("por_provedor")
                .unwrap()
                .obter("claude")
                .unwrap()
                .como_numero(),
            Some(3.0)
        );
    }
}
