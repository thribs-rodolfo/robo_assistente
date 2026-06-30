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

/// Monta a mensagem de alerta enviada ao Thiago. Concisa e acionável.
pub fn mensagem_alerta(relatorio: &Relatorio, limite: u64) -> String {
    let seq = relatorio.sequencia_atual_no_piso;
    let piso = relatorio.sucessos_no_piso();
    let total = relatorio.total_roteamentos();
    format!(
        "⚠️ Roteador: caí no piso (Ollama) {seq}x SEGUIDAS (limite de alerta: {limite}).\n\
         A cadeia de provedores bons (Claude/Groq/Gemini) está falhando em série — o robô \
         está respondendo só pelo modelo local fraco.\n\
         Verifique: token do Claude (refresh) e chaves Groq/Gemini.\n\
         No período analisado: {piso} de {total} roteamentos caíram no piso."
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
        assert!(msg.contains("4x SEGUIDAS"));
        assert!(msg.contains("limite de alerta: 3"));
        assert!(msg.contains("4 de 4"));
    }
}
