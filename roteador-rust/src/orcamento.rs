//! Orçamento de tempo TOTAL da cadeia de fallback.
//!
//! Cada provedor já tem o seu próprio `timeout`, mas nada limitava o tempo SOMADO da cadeia:
//! se o Claude trava até o timeout (por exemplo 30s) e só então o Gemini é tentado e depois o
//! Ollama (mais ~35s), o usuário espera mais de um minuto por uma resposta de chat. O orçamento
//! total resolve isso: assim que o tempo JÁ GASTO nesta mensagem passa do limite, o roteador
//! para de tentar os provedores CAROS/incertos de cima e vai DIRETO ao piso (Ollama local), que
//! é a resposta garantida. O piso NUNCA é pulado — a promessa "o robô nunca fica mudo" segue de pé.
//!
//! Complementa o disjuntor (ver [`crate::disjuntor`]): o disjuntor pula um provedor por
//! HISTÓRICO (ele vem falhando em série, em mensagens anteriores); o orçamento pula por TEMPO
//! GASTO NESTA mensagem (mesmo um provedor "saudável" que simplesmente demorou demais nesta
//! rodada). Sinais diferentes, objetivo convergente: depender menos da latência da cadeia de cima.
//!
//! LIMITAÇÃO honesta: a checagem acontece ENTRE provedores (antes de COMEÇAR o próximo). Um
//! provedor JÁ INICIADO roda até o seu próprio `timeout` — não abortamos no meio da chamada
//! (isso exigiria cancelamento de I/O, uma mudança bem maior). Ou seja, o orçamento governa se
//! vale a pena COMEÇAR o próximo provedor de cima, não interrompe o que já está em andamento.
//! Consequência: o tempo total pode passar do orçamento pela duração do provedor em curso — o
//! orçamento é um limite de "quando parar de escalar", não um relógio de parada rígido.

/// Decide se o provedor da vez deve ser PULADO por estouro do orçamento de tempo total.
///
/// - `orcamento_total_ms`: limite configurado. `None` = sem orçamento = comportamento antigo
///   (nunca pula por tempo).
/// - `decorrido_ms`: quanto tempo a cadeia já gastou NESTA mensagem (medido no relógio real
///   pelo [`crate::rotear`]).
/// - `eh_piso`: se este é o último provedor (o piso de emergência). O piso NUNCA é pulado.
///
/// Função PURA (sem relógio, sem I/O): a decisão fica testável isoladamente; quem chama só
/// fornece o tempo decorrido. Devolve `true` quando o provedor deve ser pulado.
pub fn deve_pular_por_orcamento(
    orcamento_total_ms: Option<u64>,
    decorrido_ms: u128,
    eh_piso: bool,
) -> bool {
    match orcamento_total_ms {
        // Sem orçamento configurado: nunca pula por tempo (idêntico ao comportamento antigo).
        None => false,
        // O piso é sagrado: mesmo com o orçamento estourado, ele é SEMPRE tentado — é a
        // garantia de que o robô nunca fica mudo.
        Some(_) if eh_piso => false,
        // Estourou o orçamento: pula este provedor de cima e segue para o próximo (rumo ao piso).
        Some(limite) => decorrido_ms >= u128::from(limite),
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn sem_orcamento_nunca_pula() {
        // None = desligado: mesmo com muito tempo decorrido, nada é pulado por tempo.
        assert!(!deve_pular_por_orcamento(None, 999_999, false));
        assert!(!deve_pular_por_orcamento(None, 999_999, true));
    }

    #[test]
    fn dentro_do_orcamento_nao_pula() {
        // 500ms decorridos, orçamento 1000ms: ainda há folga, tenta o provedor.
        assert!(!deve_pular_por_orcamento(Some(1000), 500, false));
    }

    #[test]
    fn estourou_o_orcamento_pula_provedor_de_cima() {
        // 1500ms decorridos, orçamento 1000ms: estourou -> pula este provedor de cima.
        assert!(deve_pular_por_orcamento(Some(1000), 1500, false));
    }

    #[test]
    fn no_limite_exato_ja_pula() {
        // Igual ao limite conta como estouro (>=): não vale a pena começar mais um provedor caro.
        assert!(deve_pular_por_orcamento(Some(1000), 1000, false));
    }

    #[test]
    fn piso_nunca_e_pulado_mesmo_estourado() {
        // Mesmo muito além do orçamento, o piso (último) é sempre tentado.
        assert!(!deve_pular_por_orcamento(Some(1000), 10_000, true));
    }

    #[test]
    fn primeiro_provedor_com_tempo_zero_nao_e_pulado() {
        // No começo da cadeia o tempo decorrido é ~0: o primeiro provedor sempre é tentado,
        // por menor que seja o orçamento (só os SEGUINTES podem ser pulados por tempo).
        assert!(!deve_pular_por_orcamento(Some(1), 0, false));
    }
}
