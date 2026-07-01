//! Retentativa em falhas transitórias, ANTES de cair para o próximo provedor.
//!
//! Motivo (o objetivo do projeto): depender MENOS do piso (Ollama). Se o provedor bom
//! sofreu um blip passageiro — a rede piscou, veio um 503/429 momentâneo — cair direto pro
//! próximo (e no fim pro piso lento e fraco) é desperdício: uma segunda tentativa logo em
//! seguida costuma passar. Aqui decidimos QUANDO retentar e QUANTO esperar entre tentativas.
//!
//! Filosofia (WORKSPACE_RULES "Como escrevemos código"): a DECISÃO é uma função pura
//! ([`planejar`]) — não dorme, não olha relógio nem rede, então é trivial de testar. Quem
//! DORME de fato é o laço fino em [`crate::rotear`], que só executa o plano.
//!
//! **Desligado por padrão:** o número de retentativas vem da config do provedor
//! (`retentativas`, padrão 0). Com 0, [`planejar`] sempre devolve `None` e o roteamento se
//! comporta EXATAMENTE como antes — risco e latência extra zero para quem não configura.

use std::time::Duration;

use crate::erro::FalhaProvedor;

/// Teto absoluto de espera entre retentativas. Estamos no caminho de uma mensagem VIVA
/// (o usuário aguardando), então mesmo com backoff exponencial nunca dormimos mais que isto.
const TETO_ESPERA_MS: u64 = 5_000;

/// Decide se vale RE-tentar e, se sim, quanto esperar antes da próxima tentativa.
///
/// - `falha`: o que deu errado na tentativa que acabou de falhar.
/// - `tentativa_atual`: quantas retentativas JÁ foram feitas (0 na 1ª falha, 1 na 2ª...).
/// - `max_retentativas`: orçamento de retentativas além da tentativa original (config).
/// - `espera_base_ms`: base do backoff exponencial entre tentativas.
///
/// Devolve `Some(espera)` quando deve retentar depois de dormir `espera`, ou `None` quando
/// deve desistir deste provedor (orçamento esgotado OU falha que não vale retentar).
///
/// Função **pura**: só aritmética e a classificação [`FalhaProvedor::vale_retentar`].
pub fn planejar(
    falha: &FalhaProvedor,
    tentativa_atual: u32,
    max_retentativas: u32,
    espera_base_ms: u64,
) -> Option<Duration> {
    // Orçamento esgotado: já usamos todas as retentativas permitidas.
    if tentativa_atual >= max_retentativas {
        return None;
    }
    // Falha não-transitória (auth, mensagem, processo, config): repetir na hora não ajuda.
    if !falha.vale_retentar() {
        return None;
    }
    Some(espera_backoff(tentativa_atual, espera_base_ms))
}

/// Espera do backoff exponencial para a retentativa de índice `tentativa`: `base · 2^tentativa`.
///
/// Ex.: base 250ms → 250, 500, 1000, 2000... A ideia é dar um respiro crescente para o
/// provedor se recuperar, sem estender demais a espera do usuário (daí o [`TETO_ESPERA_MS`]).
///
/// Aritmética SATURANTE e expoente capado: nunca estoura o `u64` nem passa do teto, por mais
/// alto que alguém configure `retentativas`.
pub fn espera_backoff(tentativa: u32, base_ms: u64) -> Duration {
    // Cap do expoente em 32: 2^32 já é gigante; acima disso o `1 << expoente` estouraria.
    let expoente = tentativa.min(32);
    let fator = 1u64.checked_shl(expoente).unwrap_or(u64::MAX);
    let ms = base_ms.saturating_mul(fator).min(TETO_ESPERA_MS);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn nao_retenta_quando_orcamento_zerado() {
        // Padrão do projeto: sem retentativas configuradas, nunca retenta (comportamento antigo).
        let falha = FalhaProvedor::Rede("piscou".into());
        assert_eq!(planejar(&falha, 0, 0, 250), None);
    }

    #[test]
    fn retenta_falha_transitoria_dentro_do_orcamento() {
        let falha = FalhaProvedor::Http {
            status: 503,
            corpo: String::new(),
        };
        // 1ª falha (tentativa_atual=0), com orçamento 2: deve retentar após a base.
        assert_eq!(
            planejar(&falha, 0, 2, 250),
            Some(Duration::from_millis(250))
        );
        // 2ª falha (tentativa_atual=1): backoff dobra.
        assert_eq!(
            planejar(&falha, 1, 2, 250),
            Some(Duration::from_millis(500))
        );
        // 3ª falha (tentativa_atual=2): orçamento esgotado -> desiste.
        assert_eq!(planejar(&falha, 2, 2, 250), None);
    }

    #[test]
    fn nao_retenta_falha_nao_transitoria_mesmo_com_orcamento() {
        // Auth não passa numa retentativa imediata, então nem gasta o orçamento.
        let auth = FalhaProvedor::Http {
            status: 401,
            corpo: String::new(),
        };
        assert_eq!(planejar(&auth, 0, 5, 250), None);
        // Processo (Claude CLI) idem — e ainda respeita "não martelar o Claude".
        let processo = FalhaProvedor::Processo("token caiu".into());
        assert_eq!(planejar(&processo, 0, 5, 250), None);
    }

    #[test]
    fn backoff_dobra_e_respeita_o_teto() {
        assert_eq!(espera_backoff(0, 250), Duration::from_millis(250));
        assert_eq!(espera_backoff(1, 250), Duration::from_millis(500));
        assert_eq!(espera_backoff(2, 250), Duration::from_millis(1000));
        // Bem acima do teto: satura em TETO_ESPERA_MS, sem estourar o u64.
        assert_eq!(
            espera_backoff(60, 250),
            Duration::from_millis(TETO_ESPERA_MS)
        );
        assert_eq!(
            espera_backoff(5, 1_000_000),
            Duration::from_millis(TETO_ESPERA_MS)
        );
    }
}
