//! Alerta automático de dependência: avisa o Thiago quando o robô cai no piso (Ollama)
//! N vezes SEGUIDAS — sinal de que a cadeia de provedores bons (Claude/Groq/Gemini) está
//! falhando em série e o robô está respondendo só pelo modelo local fraco.
//!
//! Reaproveita [`metricas::Relatorio::sequencia_atual_no_piso`]. A regra de "quando avisar /
//! quando calar" é uma FUNÇÃO PURA ([`decidir`]); o efeito colateral (rodar o notificador,
//! ler/gravar o arquivo de estado) fica no binário `bin/alerta.rs`. Assim a decisão é
//! testável sem tocar em disco nem mandar Telegram.
//!
//! Anti-spam: guardamos num arquivo de estado a maior sequência já alertada NESTA rajada.
//! Avisamos uma vez ao cruzar o limite e de novo só quando piora um "degrau" inteiro (mais
//! `limite` quedas seguidas). Quando um provedor bom volta a responder, a sequência zera e o
//! estado é limpo — a próxima rajada volta a alertar do zero.
//!
//! Filosofia (WORKSPACE_RULES "Como escrevemos código"): zero dependências, funções puras,
//! erros como valor, sem `panic`, sem erro silencioso.

use crate::metricas::Relatorio;

/// O que o alerta deve fazer depois de olhar a sequência atual de quedas no piso.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decisao {
    /// Se `true`, dispare a notificação ao Thiago.
    pub notificar: bool,
    /// Novo valor a gravar no arquivo de estado (a sequência já "coberta" por alerta).
    pub novo_estado: u64,
}

/// Decide se deve alertar.
///
/// - `sequencia_atual`: quedas seguidas no piso AGORA (de [`Relatorio::sequencia_atual_no_piso`]).
/// - `limite`: a partir de quantas quedas seguidas começa a alertar (tratado como >= 1).
/// - `ja_alertado`: maior sequência já notificada nesta rajada (0 = nenhuma; lido do estado).
///
/// Regra:
/// - Abaixo do limite: nada, e o estado zera (rajada acabou ou nem começou).
/// - No limite ou acima: alerta na primeira vez (`ja_alertado == 0`) e a cada novo degrau de
///   `limite` quedas a mais desde o último alerta. Caso contrário, fica quieto mantendo o estado
///   (para não floodar o Thiago a cada rodada do cron).
pub fn decidir(sequencia_atual: u64, limite: u64, ja_alertado: u64) -> Decisao {
    let limite = limite.max(1); // limite 0 não faz sentido; trata como 1.
    if sequencia_atual < limite {
        // Cadeia saudável (ou sem dados): some o estado, próxima rajada volta a alertar.
        return Decisao {
            notificar: false,
            novo_estado: 0,
        };
    }
    // Estamos no piso há `sequencia_atual` (>= limite) vezes seguidas.
    let primeira_vez = ja_alertado == 0;
    let novo_degrau = ja_alertado != 0 && sequencia_atual >= ja_alertado + limite;
    if primeira_vez || novo_degrau {
        Decisao {
            notificar: true,
            novo_estado: sequencia_atual,
        }
    } else {
        // Já avisamos e ainda não piorou um degrau inteiro: cala, mas mantém o estado
        // (mede o próximo degrau a partir do último alerta, não da rodada atual).
        Decisao {
            notificar: false,
            novo_estado: ja_alertado,
        }
    }
}

/// Severidade ESCALONADA do alarme de sequência. Conforme as quedas seguidas no piso passam
/// de múltiplos do `limite`, a gravidade sobe — dando ao Thiago uma triagem rápida sem precisar
/// ler o número: 🟡 ATENÇÃO (começou) < 🟠 ALERTA (piorou) < 🔴 CRÍTICO (dependência prolongada).
///
/// O escalonamento casa de propósito com o anti-spam por degraus de [`decidir`]: como cada
/// re-alerta dispara ao acumular mais `limite` quedas, cada subida de nível tende a coincidir
/// com uma nova notificação — o Thiago vê a gravidade crescer mensagem a mensagem, sem flood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severidade {
    /// Abaixo do limite — não é alarme (cadeia saudável ou rajada curta).
    Normal,
    /// Primeiro degrau: `[limite, 2*limite)`. Primeiro sinal de degradação em série.
    Atencao,
    /// Segundo degrau: `[2*limite, 3*limite)`. Cadeia de cima claramente comprometida.
    Alerta,
    /// Terceiro degrau ou mais: `>= 3*limite`. Dependência prolongada do piso fraco.
    Critico,
}

impl Severidade {
    /// Etiqueta legível (com emoji de cor) para abrir a mensagem ao Thiago e os logs.
    pub fn etiqueta(self) -> &'static str {
        match self {
            Severidade::Normal => "NORMAL",
            Severidade::Atencao => "🟡 ATENÇÃO",
            Severidade::Alerta => "🟠 ALERTA",
            Severidade::Critico => "🔴 CRÍTICO",
        }
    }
}

/// Classifica a gravidade de uma sequência de quedas no piso em função do `limite`.
///
/// Conta quantos degraus inteiros de `limite` cabem na sequência: `[limite, 2*limite)` é o
/// primeiro (ATENÇÃO), `[2*limite, 3*limite)` o segundo (ALERTA), daí em diante CRÍTICO.
/// Abaixo do limite não é alarme (NORMAL). `limite` 0 é tratado como 1 (igual a [`decidir`]).
pub fn severidade(sequencia: u64, limite: u64) -> Severidade {
    let limite = limite.max(1);
    match sequencia / limite {
        0 => Severidade::Normal,
        1 => Severidade::Atencao,
        2 => Severidade::Alerta,
        _ => Severidade::Critico,
    }
}

/// Margem de histerese (em pontos percentuais) do alarme percentual. Depois de alertar que
/// a fração no piso passou do limiar, só consideramos "recuperado" quando ela cair a
/// `limiar - MARGEM` ou menos. Sem essa folga, um percentual oscilando bem na fronteira do
/// limiar ligaria/desligaria o alarme a cada rodada do cron (flapping) e floodaria o Thiago.
pub const MARGEM_HISTERESE_PERCENTUAL: u8 = 15;

/// O que o alarme PERCENTUAL deve fazer. Espelha [`Decisao`], mas o estado anti-spam aqui é
/// um liga/desliga ("estou numa fase de alta já avisada?") em vez de um contador de degraus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisaoPercentual {
    /// Se `true`, dispare a notificação ao Thiago.
    pub notificar: bool,
    /// Novo estado a gravar: `true` = "estou numa fase de alta já alertada" (não realertar).
    pub ja_em_alta: bool,
}

/// Decide se deve alertar pela FRAÇÃO de quedas no piso numa janela — ortogonal ao alarme de
/// sequência ([`decidir`]). Pega o caso em que a cadeia de cima falha de forma intermitente
/// mas pesada (ex.: 8 de 10 roteamentos no piso) sem nunca acumular quedas seguidas suficientes.
///
/// - `piso`/`total`: quedas no piso e total de roteamentos no período (de [`Relatorio`]).
/// - `limiar_percentual`: a partir de que % no piso alertar (ex.: 70).
/// - `minimo_amostras`: abaixo de tantos roteamentos não há sinal confiável (evita "1 de 1 = 100%").
/// - `ja_em_alta`: estado anterior (lido do arquivo) — já avisamos nesta fase de alta?
///
/// Regra (com histerese [`MARGEM_HISTERESE_PERCENTUAL`] para não ficar piscando):
/// - Amostras de menos: não alerta e re-arma (zera o estado) — não dá pra concluir.
/// - `% >= limiar`: alerta só na primeira vez; depois fica armado e quieto.
/// - `% <= limiar - margem`: voltou ao normal com folga → re-arma para a próxima fase.
/// - Zona morta entre os dois: mantém o estado (nem alerta de novo, nem re-arma).
pub fn decidir_por_percentual(
    piso: u64,
    total: u64,
    limiar_percentual: u8,
    minimo_amostras: u64,
    ja_em_alta: bool,
) -> DecisaoPercentual {
    // Sem amostras suficientes não há sinal confiável: não alerta e re-arma para a próxima fase.
    if total < minimo_amostras.max(1) {
        return DecisaoPercentual {
            notificar: false,
            ja_em_alta: false,
        };
    }
    // Percentual em inteiro (comparação estável, sem float colado na fronteira do limiar).
    let pct = (piso as u128 * 100 / total as u128) as u64;
    let limiar = limiar_percentual as u64;
    let recuperacao = limiar_percentual.saturating_sub(MARGEM_HISTERESE_PERCENTUAL) as u64;

    if pct >= limiar {
        // Em alta: alerta na primeira vez; nas próximas rodadas fica armado e calado.
        DecisaoPercentual {
            notificar: !ja_em_alta,
            ja_em_alta: true,
        }
    } else if pct <= recuperacao {
        // Caiu bem abaixo do limiar: fase de alta acabou, re-arma para a próxima.
        DecisaoPercentual {
            notificar: false,
            ja_em_alta: false,
        }
    } else {
        // Zona morta (entre recuperação e limiar): segura o estado atual para não flapar.
        DecisaoPercentual {
            notificar: false,
            ja_em_alta,
        }
    }
}

/// Monta a mensagem de alerta enviada ao Thiago. Concisa e acionável.
pub fn mensagem_alerta(relatorio: &Relatorio, limite: u64) -> String {
    let seq = relatorio.sequencia_atual_no_piso;
    let piso = relatorio.sucessos_no_piso();
    let total = relatorio.total_roteamentos();
    let etiqueta = severidade(seq, limite).etiqueta();
    format!(
        "{etiqueta} Roteador: caí no piso (Ollama) {seq}x SEGUIDAS (limite de alerta: {limite}).\n\
         A cadeia de provedores bons (Claude/Groq/Gemini) está falhando em série — o robô \
         está respondendo só pelo modelo local fraco.\n\
         Verifique: token do Claude (refresh) e chaves Groq/Gemini.\n\
         No período analisado: {piso} de {total} roteamentos caíram no piso."
    )
}

/// Monta a mensagem do alarme PERCENTUAL. `janela` é a descrição legível do período analisado
/// (ex.: "24h") ou `None` para "todo o histórico".
pub fn mensagem_alerta_percentual(
    relatorio: &Relatorio,
    limiar_percentual: u8,
    janela: Option<&str>,
) -> String {
    let piso = relatorio.sucessos_no_piso();
    let total = relatorio.total_roteamentos();
    let pct = relatorio.percentual_no_piso();
    let periodo = match janela {
        Some(j) => format!("nas últimas {j}"),
        None => "em todo o histórico".to_string(),
    };
    format!(
        "⚠️ Roteador: {pct:.0}% dos roteamentos {periodo} caíram no piso (Ollama) — \
         {piso} de {total} (limiar de alerta: {limiar_percentual}%).\n\
         Mesmo sem quedas longas em série, a dependência do modelo local fraco está alta: \
         a cadeia de provedores bons (Claude/Groq/Gemini) está respondendo pouco.\n\
         Verifique: token do Claude (refresh) e chaves Groq/Gemini."
    )
}

/// Interpreta o conteúdo do arquivo de estado (um único número) na maior sequência já alertada.
///
/// Conteúdo vazio -> 0 (nenhuma rajada alertada; caso normal de primeira execução). Conteúdo
/// não-numérico devolve `Err` — quem chama decide (o binário loga o aviso e recomeça do zero,
/// nunca em silêncio).
pub fn parsear_estado(conteudo: &str) -> Result<u64, std::num::ParseIntError> {
    let texto = conteudo.trim();
    if texto.is_empty() {
        return Ok(0);
    }
    texto.parse()
}

/// Serializa o estado para gravar no arquivo (só o número + quebra de linha).
pub fn serializar_estado(ja_alertado: u64) -> String {
    format!("{ja_alertado}\n")
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn abaixo_do_limite_nao_alerta_e_zera_estado() {
        // 2 quedas, limite 3: ainda não é alarme. Estado volta a 0 (rajada não começou).
        assert_eq!(
            decidir(2, 3, 0),
            Decisao {
                notificar: false,
                novo_estado: 0
            }
        );
        // Mesmo que houvesse alerta antigo no estado, cair abaixo do limite limpa tudo.
        assert_eq!(
            decidir(1, 3, 9),
            Decisao {
                notificar: false,
                novo_estado: 0
            }
        );
    }

    #[test]
    fn primeira_vez_no_limite_alerta() {
        assert_eq!(
            decidir(3, 3, 0),
            Decisao {
                notificar: true,
                novo_estado: 3
            }
        );
    }

    #[test]
    fn nao_realerta_enquanto_nao_piora_um_degrau() {
        // Já alertamos em 3 (limite 3). Em 4 e 5 ainda não completou outro degrau (3+3=6).
        assert_eq!(
            decidir(4, 3, 3),
            Decisao {
                notificar: false,
                novo_estado: 3
            }
        );
        assert_eq!(
            decidir(5, 3, 3),
            Decisao {
                notificar: false,
                novo_estado: 3
            }
        );
    }

    #[test]
    fn realerta_ao_completar_novo_degrau() {
        // 3 + 3 = 6: piorou um degrau inteiro desde o último alerta -> avisa de novo.
        assert_eq!(
            decidir(6, 3, 3),
            Decisao {
                notificar: true,
                novo_estado: 6
            }
        );
    }

    #[test]
    fn recuperacao_zera_para_proxima_rajada() {
        // Provedor bom respondeu -> sequência 0 -> estado volta a 0 (não alerta).
        assert_eq!(
            decidir(0, 3, 6),
            Decisao {
                notificar: false,
                novo_estado: 0
            }
        );
    }

    #[test]
    fn limite_zero_e_tratado_como_um() {
        // Com limite 0 (sem sentido), 1 queda já alerta — como se limite fosse 1.
        assert_eq!(
            decidir(1, 0, 0),
            Decisao {
                notificar: true,
                novo_estado: 1
            }
        );
    }

    #[test]
    fn parseia_e_serializa_estado() {
        assert_eq!(parsear_estado(""), Ok(0));
        assert_eq!(parsear_estado("   \n"), Ok(0));
        assert_eq!(parsear_estado("5"), Ok(5));
        assert_eq!(parsear_estado("  3 \n"), Ok(3));
        assert!(parsear_estado("lixo").is_err());
        assert_eq!(serializar_estado(7), "7\n");
    }

    #[test]
    fn percentual_amostras_de_menos_nao_alerta_e_rearma() {
        // 3 de 3 no piso = 100%, mas com mínimo de amostras 8 não dá pra concluir nada.
        assert_eq!(
            decidir_por_percentual(3, 3, 70, 8, false),
            DecisaoPercentual {
                notificar: false,
                ja_em_alta: false
            }
        );
        // Mesmo já estando "em alta", cair para poucas amostras re-arma (zera o estado).
        assert_eq!(
            decidir_por_percentual(2, 2, 70, 8, true),
            DecisaoPercentual {
                notificar: false,
                ja_em_alta: false
            }
        );
    }

    #[test]
    fn percentual_cruza_o_limiar_alerta_uma_vez() {
        // 8 de 10 = 80% >= limiar 70, primeira vez (não estava em alta) -> alerta.
        assert_eq!(
            decidir_por_percentual(8, 10, 70, 8, false),
            DecisaoPercentual {
                notificar: true,
                ja_em_alta: true
            }
        );
        // Continua alto na próxima rodada: fica armado e quieto (anti-spam).
        assert_eq!(
            decidir_por_percentual(9, 10, 70, 8, true),
            DecisaoPercentual {
                notificar: false,
                ja_em_alta: true
            }
        );
    }

    #[test]
    fn percentual_zona_morta_mantem_o_estado() {
        // Limiar 70, margem 15 -> recuperação em 55%. 60% está na zona morta: não realerta,
        // mas também não re-arma (segura o estado para não ficar piscando).
        assert_eq!(
            decidir_por_percentual(6, 10, 70, 8, true),
            DecisaoPercentual {
                notificar: false,
                ja_em_alta: true
            }
        );
        // A mesma zona morta, mas sem estar em alta antes: continua sem alertar.
        assert_eq!(
            decidir_por_percentual(6, 10, 70, 8, false),
            DecisaoPercentual {
                notificar: false,
                ja_em_alta: false
            }
        );
    }

    #[test]
    fn percentual_recupera_com_folga_rearma() {
        // 50% <= 55% (recuperação): fase de alta acabou, re-arma para a próxima.
        assert_eq!(
            decidir_por_percentual(5, 10, 70, 8, true),
            DecisaoPercentual {
                notificar: false,
                ja_em_alta: false
            }
        );
    }

    #[test]
    fn mensagem_percentual_traz_fracao_periodo_e_limiar() {
        // 3 de 4 no piso = 75%; janela "24h".
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'claude' em 500ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 31000ms
2026-06-30 12:03:00 UTC [roteador] [ok] respondido por 'ollama_local' em 32000ms
";
        let relatorio = crate::metricas::agregar(log);
        let msg = mensagem_alerta_percentual(&relatorio, 70, Some("24h"));
        assert!(msg.contains("75% dos roteamentos nas últimas 24h"));
        assert!(msg.contains("3 de 4"));
        assert!(msg.contains("limiar de alerta: 70%"));
        // Sem janela, descreve "todo o histórico".
        let msg_total = mensagem_alerta_percentual(&relatorio, 70, None);
        assert!(msg_total.contains("em todo o histórico"));
    }

    #[test]
    fn mensagem_traz_sequencia_e_limite() {
        // Monta um relatório com 4 quedas seguidas no piso para conferir o texto.
        let log = "\
2026-06-30 12:00:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:01:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:02:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
2026-06-30 12:03:00 UTC [roteador] [ok] respondido por 'ollama_local' em 30000ms
";
        let relatorio = crate::metricas::agregar(log);
        let msg = mensagem_alerta(&relatorio, 3);
        // 4 quedas, limite 3 -> primeiro degrau -> ATENÇÃO abre a mensagem.
        assert!(msg.starts_with("🟡 ATENÇÃO"));
        assert!(msg.contains("4x SEGUIDAS"));
        assert!(msg.contains("limite de alerta: 3"));
        assert!(msg.contains("4 de 4"));
    }

    #[test]
    fn severidade_escalona_por_degrau() {
        // Limite 3: abaixo do limite é NORMAL (não é alarme).
        assert_eq!(severidade(0, 3), Severidade::Normal);
        assert_eq!(severidade(2, 3), Severidade::Normal);
        // Primeiro degrau [3, 6): ATENÇÃO.
        assert_eq!(severidade(3, 3), Severidade::Atencao);
        assert_eq!(severidade(5, 3), Severidade::Atencao);
        // Segundo degrau [6, 9): ALERTA.
        assert_eq!(severidade(6, 3), Severidade::Alerta);
        assert_eq!(severidade(8, 3), Severidade::Alerta);
        // Terceiro degrau ou mais (>= 9): CRÍTICO.
        assert_eq!(severidade(9, 3), Severidade::Critico);
        assert_eq!(severidade(100, 3), Severidade::Critico);
    }

    #[test]
    fn severidade_limite_zero_tratado_como_um() {
        // Igual a `decidir`: limite 0 vira 1. 1 queda já é o primeiro degrau (ATENÇÃO),
        // 2 o segundo (ALERTA), 3+ CRÍTICO.
        assert_eq!(severidade(0, 0), Severidade::Normal);
        assert_eq!(severidade(1, 0), Severidade::Atencao);
        assert_eq!(severidade(2, 0), Severidade::Alerta);
        assert_eq!(severidade(3, 0), Severidade::Critico);
    }

    #[test]
    fn severidade_acompanha_os_realertas_por_degrau() {
        // O escalonamento deve subir EXATAMENTE quando `decidir` re-alerta um novo degrau,
        // para cada notificação levar uma severidade maior que a anterior. Limite 3:
        // alerta em 3 (ATENÇÃO), re-alerta em 6 (ALERTA) e em 9 (CRÍTICO).
        assert!(decidir(3, 3, 0).notificar);
        assert_eq!(severidade(3, 3), Severidade::Atencao);
        assert!(decidir(6, 3, 3).notificar);
        assert_eq!(severidade(6, 3), Severidade::Alerta);
        assert!(decidir(9, 3, 6).notificar);
        assert_eq!(severidade(9, 3), Severidade::Critico);
    }

    #[test]
    fn etiquetas_de_severidade() {
        assert_eq!(Severidade::Normal.etiqueta(), "NORMAL");
        assert_eq!(Severidade::Atencao.etiqueta(), "🟡 ATENÇÃO");
        assert_eq!(Severidade::Alerta.etiqueta(), "🟠 ALERTA");
        assert_eq!(Severidade::Critico.etiqueta(), "🔴 CRÍTICO");
    }
}
