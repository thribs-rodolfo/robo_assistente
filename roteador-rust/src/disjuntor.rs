//! Disjuntor (circuit breaker) por provedor — "melhor roteamento" de verdade.
//!
//! # Por que isto existe
//! O robô nasceu porque o Claude é FRÁGIL: o token OAuth cai de tempos em tempos. Quando
//! isso acontece, o roteamento linear (ver [`crate::rotear`]) tenta o Claude PRIMEIRO em
//! TODA mensagem, paga o timeout inteiro (`claude --print` demora a falhar), e só então
//! cai pro Ollama. Ou seja: enquanto o Claude está fora, cada mensagem fica lenta à toa.
//!
//! O disjuntor corta esse desperdício. Ele lembra as falhas RECENTES de cada provedor:
//! depois de `limiar_falhas` falhas seguidas, "abre o circuito" daquele provedor por um
//! `cooldown` — nesse intervalo o roteador PULA o provedor sem gastar rede/processo, indo
//! direto pro próximo. Passado o cooldown, ele deixa passar UMA tentativa ("meio-aberto"):
//! se der certo, fecha o circuito; se falhar de novo, reabre. É o padrão clássico de
//! circuit breaker, escrito à mão (zero dependências).
//!
//! # Segurança de projeto (nunca ficar mudo)
//! O piso (Ollama local, SEMPRE o último da `ordem_fallback`) NUNCA é pulado pelo disjuntor
//! — quem decide isso é o [`crate::rotear`], que só consulta o disjuntor para os provedores
//! que NÃO são o último. Assim, mesmo com todos os circuitos abertos, o robô responde.
//!
//! # Estado é operacional e efêmero
//! Diferente de "histórico de conversa" (que muda o que o bot DIZ), o estado do disjuntor
//! só afeta a EFICIÊNCIA do roteamento. Se o arquivo de estado sumir ou vier corrompido,
//! tratamos tudo como "fechado" (comportamento antigo, idêntico) — degrada com graça.
//!
//! # Testável
//! Toda a lógica de decisão é função pura sobre `(estado, agora, config)` — dá pra testar
//! sem relógio, sem disco e sem rede. A persistência é best-effort e testada com arquivo
//! temporário.

use std::collections::BTreeMap;
use std::io::Write;

use crate::config::ConfigDisjuntor;
use crate::json::{self, Valor};

/// Estado de um provedor no disjuntor. "Fechado" (saudável) é o estado natural:
/// `falhas_consecutivas == 0` e `aberto_ate == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EstadoProvedor {
    /// Quantas falhas seguidas (sem nenhum sucesso no meio) este provedor acumulou.
    pub falhas_consecutivas: u32,
    /// Instante (epoch em segundos UTC) até o qual o circuito fica ABERTO. `0` = fechado.
    /// Enquanto `agora < aberto_ate`, o roteador pula o provedor.
    pub aberto_ate: u64,
}

/// O estado do disjuntor para todos os provedores. Mapa nome->estado.
///
/// Usamos `BTreeMap` (e não `HashMap`) de propósito: a ordem fica determinística, então a
/// serialização é estável e os testes ficam previsíveis. É stdlib — zero dependências.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EstadoDisjuntor {
    por_provedor: BTreeMap<String, EstadoProvedor>,
}

impl EstadoDisjuntor {
    /// Estado inicial: nenhum provedor conhecido, todos os circuitos fechados.
    pub fn vazio() -> Self {
        Self::default()
    }

    /// O circuito deste provedor está ABERTO neste instante? (Aberto = deve pular.)
    ///
    /// Provedor desconhecido conta como fechado (nunca falhou ainda). O cooldown expira
    /// sozinho: quando `agora` alcança `aberto_ate`, volta a ser `false` — é aí que o
    /// roteador deixa passar a tentativa de "meio-aberto".
    pub fn esta_aberto(&self, nome: &str, agora: u64) -> bool {
        match self.por_provedor.get(nome) {
            Some(estado) => agora < estado.aberto_ate,
            None => false,
        }
    }

    /// Registra uma FALHA real (o provedor foi tentado e o `responder` deu erro).
    ///
    /// Incrementa o contador de falhas seguidas; se cruzar `limiar_falhas`, abre o circuito
    /// a partir de `agora` por um cooldown com BACKOFF EXPONENCIAL (ver [`cooldown_com_backoff`]).
    /// Note que, no "meio-aberto", o contador já está em/acima do limiar — então uma única
    /// falha reabre imediatamente, e cada reabertura seguida fica MAIS longa (dobra até o teto):
    /// um provedor morto há horas deixa de ser sondado toda hora, sem nunca ser esquecido.
    pub fn apos_falha(&mut self, nome: &str, agora: u64, config: &ConfigDisjuntor) {
        let estado = self.por_provedor.entry(nome.to_string()).or_default();
        estado.falhas_consecutivas = estado.falhas_consecutivas.saturating_add(1);
        if estado.falhas_consecutivas >= config.limiar_falhas {
            let cooldown = cooldown_com_backoff(estado.falhas_consecutivas, config);
            estado.aberto_ate = agora.saturating_add(cooldown);
        }
    }

    /// Registra um SUCESSO: fecha o circuito e zera as falhas. Provedor voltou a si.
    ///
    /// Removemos a entrada em vez de guardar um estado "zerado": mantém o mapa (e o arquivo
    /// de estado) enxuto, contendo só os provedores que estão de fato com problema.
    pub fn apos_sucesso(&mut self, nome: &str) {
        self.por_provedor.remove(nome);
    }

    /// Quantas falhas seguidas este provedor acumulou (0 se desconhecido). Para telemetria.
    pub fn falhas_de(&self, nome: &str) -> u32 {
        self.por_provedor
            .get(nome)
            .map(|e| e.falhas_consecutivas)
            .unwrap_or(0)
    }

    /// Serializa o estado como JSON (nosso próprio encoder). Formato:
    /// `{"claude": {"falhas_consecutivas": 3, "aberto_ate": 1751000060}, ...}`.
    pub fn para_json(&self) -> String {
        let pares = self
            .por_provedor
            .iter()
            .map(|(nome, estado)| {
                let objeto = Valor::Objeto(vec![
                    (
                        "falhas_consecutivas".into(),
                        Valor::Numero(estado.falhas_consecutivas as f64),
                    ),
                    ("aberto_ate".into(), Valor::Numero(estado.aberto_ate as f64)),
                ]);
                (nome.clone(), objeto)
            })
            .collect::<Vec<_>>();
        Valor::Objeto(pares).para_texto()
    }

    /// Interpreta o estado a partir de JSON. Tolerante: qualquer coisa fora do formato
    /// (não é objeto, campos faltando, números negativos) vira `None` — o chamador então
    /// usa [`EstadoDisjuntor::vazio`]. Nunca entra em pânico, nunca inventa estado errado.
    pub fn de_json(texto: &str) -> Option<Self> {
        let raiz = json::parsear(texto).ok()?;
        let pares = match raiz {
            Valor::Objeto(pares) => pares,
            _ => return None,
        };

        let mut por_provedor = BTreeMap::new();
        for (nome, valor) in pares {
            // Números vêm como f64; convertemos com piso em 0 (nunca negativo).
            let falhas = valor
                .obter("falhas_consecutivas")
                .and_then(Valor::como_numero)?;
            let aberto_ate = valor.obter("aberto_ate").and_then(Valor::como_numero)?;
            if falhas < 0.0 || aberto_ate < 0.0 {
                return None;
            }
            por_provedor.insert(
                nome,
                EstadoProvedor {
                    falhas_consecutivas: falhas as u32,
                    aberto_ate: aberto_ate as u64,
                },
            );
        }
        Some(Self { por_provedor })
    }

    /// Lê o estado do arquivo. Best-effort e tolerante: arquivo ausente (primeira vez) ou
    /// corrompido -> estado vazio (todos os circuitos fechados = comportamento antigo).
    ///
    /// Não é "erro silencioso": um arquivo que EXISTE mas está corrompido gera um aviso no
    /// stderr — só que não derruba o roteamento (o estado do disjuntor é operacional, não
    /// pode impedir o robô de responder).
    pub fn carregar(caminho: &str) -> Self {
        match std::fs::read_to_string(caminho) {
            Ok(conteudo) => match Self::de_json(&conteudo) {
                Some(estado) => estado,
                None => {
                    eprintln!("[disjuntor] estado corrompido em {caminho}: começando zerado");
                    Self::vazio()
                }
            },
            // Ausente é o caso NORMAL na primeira execução; não polui o stderr.
            Err(_) => Self::vazio(),
        }
    }

    /// Grava o estado no arquivo (best-effort). Falha de escrita avisa no stderr mas não
    /// derruba o roteamento — pior caso, na próxima mensagem relemos o estado anterior.
    pub fn salvar(&self, caminho: &str) {
        let resultado = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(caminho)
            .and_then(|mut arquivo| arquivo.write_all(self.para_json().as_bytes()));
        if let Err(erro) = resultado {
            eprintln!("[disjuntor] não consegui gravar estado em {caminho}: {erro}");
        }
    }
}

/// Calcula por quantos segundos o circuito deve ficar aberto, dado o total de falhas
/// seguidas — cooldown BASE com backoff exponencial, limitado por um teto.
///
/// A ideia: a PRIMEIRA abertura (falhas == limiar) usa o cooldown base; cada falha SEGUINTE
/// (reabertura no meio-aberto) DOBRA o intervalo — base, 2×base, 4×base… — até `cooldown_maximo`.
/// Assim um provedor que volta rápido sofre pouca espera, mas um que insiste em falhar (token
/// do Claude caído há horas) para de ser sondado a cada cooldown fixo, economizando latência.
///
/// Função PURA (só faz conta sobre os números) → testável sem relógio/disco/rede. Tudo com
/// aritmética saturante e teto: nunca estoura `u64`, nunca fica aberto "para sempre". Se o teto
/// vier menor que o base (config estranha), o base vira o piso — nunca devolvemos menos que ele.
pub fn cooldown_com_backoff(falhas_consecutivas: u32, config: &ConfigDisjuntor) -> u64 {
    // Quantas falhas ALÉM do limiar já houve. 0 na primeira abertura, 1 na primeira reabertura…
    // Limitamos o expoente a 32: 2^32 já satura qualquer cooldown real, e evita expoente absurdo.
    let excedente = falhas_consecutivas
        .saturating_sub(config.limiar_falhas)
        .min(32);
    let fator = 2u64.saturating_pow(excedente);
    let com_backoff = config.cooldown_segundos.saturating_mul(fator);
    // O teto nunca pode ser menor que o base (defesa contra config invertida).
    let teto = config
        .cooldown_maximo_segundos
        .max(config.cooldown_segundos);
    com_backoff.min(teto)
}

#[cfg(test)]
mod testes {
    use super::*;

    /// Config de teste: abre com 3 falhas, cooldown base 60s, teto 600s.
    fn config_teste() -> ConfigDisjuntor {
        ConfigDisjuntor {
            habilitado: true,
            limiar_falhas: 3,
            cooldown_segundos: 60,
            cooldown_maximo_segundos: 600,
            caminho_estado: "/tmp/nao-usado.estado".into(),
        }
    }

    #[test]
    fn provedor_desconhecido_esta_fechado() {
        let estado = EstadoDisjuntor::vazio();
        assert!(!estado.esta_aberto("claude", 1_000));
    }

    #[test]
    fn abre_apos_atingir_o_limiar() {
        let cfg = config_teste();
        let mut estado = EstadoDisjuntor::vazio();

        // Duas falhas: ainda fechado (limiar é 3).
        estado.apos_falha("claude", 1_000, &cfg);
        estado.apos_falha("claude", 1_001, &cfg);
        assert!(!estado.esta_aberto("claude", 1_002));
        assert_eq!(estado.falhas_de("claude"), 2);

        // Terceira falha em t=1_002: abre até 1_002 + 60 = 1_062.
        estado.apos_falha("claude", 1_002, &cfg);
        assert!(estado.esta_aberto("claude", 1_002));
        assert!(estado.esta_aberto("claude", 1_061));
        // Passado o cooldown, volta a deixar passar (meio-aberto).
        assert!(!estado.esta_aberto("claude", 1_062));
    }

    #[test]
    fn sucesso_fecha_o_circuito() {
        let cfg = config_teste();
        let mut estado = EstadoDisjuntor::vazio();
        for t in 0..3 {
            estado.apos_falha("claude", t, &cfg);
        }
        assert!(estado.esta_aberto("claude", 2));

        estado.apos_sucesso("claude");
        assert!(!estado.esta_aberto("claude", 2));
        assert_eq!(estado.falhas_de("claude"), 0);
    }

    #[test]
    fn meio_aberto_falha_reabre_na_hora() {
        let cfg = config_teste();
        let mut estado = EstadoDisjuntor::vazio();
        for t in 0..3 {
            estado.apos_falha("claude", t, &cfg);
        }
        // Cooldown expirou (aberto_ate = 2 + 60 = 62); em t=62 está meio-aberto.
        assert!(!estado.esta_aberto("claude", 62));
        // Tentou e falhou de novo: já está acima do limiar, então reabre imediatamente.
        estado.apos_falha("claude", 62, &cfg);
        assert!(estado.esta_aberto("claude", 62));
        assert!(estado.esta_aberto("claude", 121));
    }

    #[test]
    fn json_ida_e_volta() {
        let cfg = config_teste();
        let mut estado = EstadoDisjuntor::vazio();
        for t in 0..4 {
            estado.apos_falha("claude", t, &cfg);
        }
        estado.apos_falha("gemini", 10, &cfg);

        let texto = estado.para_json();
        let voltou = EstadoDisjuntor::de_json(&texto).unwrap();
        assert_eq!(estado, voltou);
    }

    #[test]
    fn de_json_tolera_lixo() {
        assert_eq!(EstadoDisjuntor::de_json("não é json"), None);
        assert_eq!(EstadoDisjuntor::de_json("[1,2,3]"), None); // não é objeto
                                                               // Campo faltando -> None (não inventa 0).
        assert_eq!(
            EstadoDisjuntor::de_json(r#"{"x":{"falhas_consecutivas":1}}"#),
            None
        );
        // Número negativo -> None.
        assert_eq!(
            EstadoDisjuntor::de_json(r#"{"x":{"falhas_consecutivas":-1,"aberto_ate":0}}"#),
            None
        );
    }

    #[test]
    fn objeto_vazio_vira_estado_vazio() {
        assert_eq!(
            EstadoDisjuntor::de_json("{}"),
            Some(EstadoDisjuntor::vazio())
        );
    }

    #[test]
    fn carregar_arquivo_ausente_da_vazio() {
        let estado = EstadoDisjuntor::carregar("/tmp/roteador-disjuntor-inexistente-xyz.estado");
        assert_eq!(estado, EstadoDisjuntor::vazio());
    }

    #[test]
    fn salvar_e_carregar_arquivo() {
        let caminho = std::env::temp_dir().join("roteador-disjuntor-teste.estado");
        let caminho = caminho.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&caminho);

        let cfg = config_teste();
        let mut estado = EstadoDisjuntor::vazio();
        for t in 0..3 {
            estado.apos_falha("claude", t, &cfg);
        }
        estado.salvar(&caminho);

        let carregado = EstadoDisjuntor::carregar(&caminho);
        assert_eq!(estado, carregado);
        assert!(carregado.esta_aberto("claude", 5));

        let _ = std::fs::remove_file(&caminho);
    }

    #[test]
    fn cooldown_backoff_dobra_a_cada_falha_ate_o_teto() {
        let cfg = config_teste(); // base 60, limiar 3, teto 600
                                  // Antes do limiar não abriria; a função é consultada só a partir do limiar.
        assert_eq!(cooldown_com_backoff(3, &cfg), 60); // 1ª abertura: base
        assert_eq!(cooldown_com_backoff(4, &cfg), 120); // reabertura: 2×
        assert_eq!(cooldown_com_backoff(5, &cfg), 240); // 4×
        assert_eq!(cooldown_com_backoff(6, &cfg), 480); // 8×
        assert_eq!(cooldown_com_backoff(7, &cfg), 600); // 16×=960 -> capado no teto 600
        assert_eq!(cooldown_com_backoff(50, &cfg), 600); // muito acima: segue no teto
    }

    #[test]
    fn cooldown_backoff_com_teto_menor_que_base_usa_o_base_como_piso() {
        // Config "invertida" (teto < base): não devolve menos que o base — degrada com graça.
        let cfg = ConfigDisjuntor {
            cooldown_segundos: 100,
            cooldown_maximo_segundos: 10,
            ..config_teste()
        };
        assert_eq!(cooldown_com_backoff(3, &cfg), 100);
        assert_eq!(cooldown_com_backoff(9, &cfg), 100);
    }

    #[test]
    fn apos_falha_aplica_backoff_crescente_na_reabertura() {
        let cfg = config_teste(); // base 60, limiar 3, teto 600
        let mut estado = EstadoDisjuntor::vazio();

        // Três falhas em t=0,0,0 abrem pela 1ª vez: base 60 -> aberto até 60.
        for _ in 0..3 {
            estado.apos_falha("claude", 0, &cfg);
        }
        assert!(estado.esta_aberto("claude", 59));
        assert!(!estado.esta_aberto("claude", 60)); // meio-aberto

        // Falha no meio-aberto (t=60): 4ª falha -> backoff 2× = 120 -> aberto até 180.
        estado.apos_falha("claude", 60, &cfg);
        assert!(estado.esta_aberto("claude", 179));
        assert!(!estado.esta_aberto("claude", 180));

        // Nova falha no meio-aberto (t=180): 5ª falha -> 4× = 240 -> aberto até 420.
        estado.apos_falha("claude", 180, &cfg);
        assert!(estado.esta_aberto("claude", 419));
        assert!(!estado.esta_aberto("claude", 420));
    }

    #[test]
    fn sucesso_zera_o_backoff() {
        // Depois de um sucesso, o contador some e a próxima rajada recomeça no cooldown base.
        let cfg = config_teste();
        let mut estado = EstadoDisjuntor::vazio();
        for _ in 0..5 {
            estado.apos_falha("claude", 0, &cfg); // já subiu o backoff
        }
        estado.apos_sucesso("claude");
        assert_eq!(estado.falhas_de("claude"), 0);

        // Nova rajada: volta ao base 60 (não continua do backoff alto anterior).
        for _ in 0..3 {
            estado.apos_falha("claude", 1_000, &cfg);
        }
        assert!(estado.esta_aberto("claude", 1_059));
        assert!(!estado.esta_aberto("claude", 1_060));
    }
}
