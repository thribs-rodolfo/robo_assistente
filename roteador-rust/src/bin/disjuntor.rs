//! Binário que INSPECIONA o estado do disjuntor (circuit breaker) do roteador.
//!
//! # Por que existe
//! O disjuntor ([`roteador::disjuntor`]) lembra as falhas recentes de cada provedor e
//! "abre o circuito" de quem está caindo, para o roteador PULAR aquele provedor sem pagar
//! a latência de um morto (o token do Claude cai e, sem disjuntor, cada mensagem paga o
//! timeout inteiro do `claude --print`). Só que o estado dele vive num arquivo JSON opaco:
//! um operador não tem como ver "quais circuitos estão abertos agora e por quanto tempo".
//! Sem essa visibilidade, ligar o disjuntor em produção dá medo. Este binário é o olho:
//! lê o arquivo de estado e mostra, por provedor, se está aberto (pulando) e o restante do
//! cooldown.
//!
//! # Segurança
//! É SÓ LEITURA: lê a config (para achar o caminho do estado) e o arquivo de estado. NUNCA
//! constrói provedor, NUNCA abre socket, NUNCA dispara o Claude (licao-refresh-token-rotativo).
//!
//! Uso:
//!   disjuntor                         # inspeciona o estado da config padrão (/root/.secrets/...)
//!   disjuntor /tmp/outra-config.json  # usa o caminho de estado de outra config
//!
//! Código de saída (útil em cron/monitoramento):
//!   0  nenhum circuito aberto (roteamento seguindo a ordem normal)
//!   1  ao menos um provedor está sendo pulado agora (algum circuito aberto)
//!   2  erro de uso / não consegui ler a config

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use roteador::config;
use roteador::disjuntor::EstadoDisjuntor;
use roteador::duracao::descrever_aproximada;

fn main() -> ExitCode {
    let argumentos: Vec<String> = std::env::args().skip(1).collect();
    let caminho_config = match interpretar_argumentos(&argumentos) {
        Ok(caminho) => caminho,
        Err(msg) => {
            eprintln!("[disjuntor] {msg}");
            return ExitCode::from(2);
        }
    };

    let config = match config::carregar_de_arquivo(&caminho_config) {
        Ok(config) => config,
        Err(erro) => {
            eprintln!("[disjuntor] não carreguei '{caminho_config}': {erro}");
            return ExitCode::from(2);
        }
    };

    // "Agora" para calcular o cooldown restante. Se o relógio falhar (improvável), avisamos
    // e seguimos com 0 — o pior caso é mostrar tudo como "já expirado", nunca um panic.
    let agora = agora_epoch().unwrap_or_else(|| {
        eprintln!("[disjuntor] relógio indisponível; mostrando cooldowns como expirados");
        0
    });

    let caminho_estado = &config.disjuntor.caminho_estado;
    let estado = EstadoDisjuntor::carregar(caminho_estado);

    let relatorio = montar_relatorio(&config.disjuntor, &estado, agora);
    print!("{relatorio}");

    // Código de saída: 1 se algo está sendo pulado agora, 0 se a cadeia está limpa.
    if estado.algum_aberto(agora) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Monta o texto do relatório (função PURA → testável sem disco/relógio).
///
/// Recebe a config do disjuntor (para dizer se ele está ligado) e o estado já carregado.
fn montar_relatorio(
    config: &config::ConfigDisjuntor,
    estado: &EstadoDisjuntor,
    agora: u64,
) -> String {
    let mut saida = String::new();
    saida.push_str("== Disjuntor do roteador ==\n");

    // Aviso importante: se o disjuntor está DESLIGADO na config, o estado abaixo é só
    // resíduo/histórico — o roteador não está pulando ninguém por causa dele.
    if config.habilitado {
        saida.push_str(&format!(
            "estado: LIGADO (abre com {} falhas seguidas, cooldown base {})\n",
            config.limiar_falhas,
            descrever_aproximada(config.cooldown_segundos),
        ));
    } else {
        saida.push_str(
            "estado: DESLIGADO na config — as linhas abaixo são resíduo, não afetam o roteamento\n",
        );
    }

    let resumo = estado.resumo(agora);
    if resumo.is_empty() {
        saida.push_str("Todos os circuitos fechados: nenhum provedor com falhas recentes.\n");
        return saida;
    }

    for linha in &resumo {
        if linha.aberto {
            saida.push_str(&format!(
                "  🔴 {}: ABERTO (pulado) — reabre em {} · {} falha(s) seguida(s)\n",
                linha.nome,
                descrever_aproximada(linha.segundos_restantes),
                linha.falhas_consecutivas,
            ));
        } else {
            // Fechado mas com falhas contadas: ou está abaixo do limiar, ou em "meio-aberto"
            // (cooldown expirou, a próxima tentativa decide fechar de vez ou reabrir).
            saida.push_str(&format!(
                "  🟢 {}: fechado (deixa passar) · {} falha(s) acumulada(s)\n",
                linha.nome, linha.falhas_consecutivas,
            ));
        }
    }
    saida
}

/// Interpreta os argumentos: um caminho opcional de config (default: [`config::CAMINHO_PADRAO`]).
/// Devolve `Err(mensagem)` em uso inválido — sem `panic`, sem erro silencioso.
fn interpretar_argumentos(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok(config::CAMINHO_PADRAO.to_string()),
        [caminho] if !caminho.starts_with('-') => Ok(caminho.clone()),
        [outro] => Err(format!("opção desconhecida: {outro}")),
        _ => Err("uso: disjuntor [caminho-da-config]".to_string()),
    }
}

/// Epoch atual em segundos (UTC). `None` se o relógio estiver antes de 1970 (não deve ocorrer).
fn agora_epoch() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod testes {
    use super::*;
    use roteador::config::{ConfigDisjuntor, CAMINHO_ESTADO_DISJUNTOR_PADRAO};

    fn cfg_ligado() -> ConfigDisjuntor {
        ConfigDisjuntor {
            habilitado: true,
            limiar_falhas: 3,
            cooldown_segundos: 60,
            cooldown_maximo_segundos: 600,
            caminho_estado: CAMINHO_ESTADO_DISJUNTOR_PADRAO.to_string(),
        }
    }

    #[test]
    fn sem_argumento_usa_caminho_padrao() {
        assert_eq!(interpretar_argumentos(&[]).unwrap(), config::CAMINHO_PADRAO);
    }

    #[test]
    fn rejeita_opcao_e_argumentos_extras() {
        assert!(interpretar_argumentos(&["--xpto".into()]).is_err());
        assert!(interpretar_argumentos(&["a".into(), "b".into()]).is_err());
    }

    #[test]
    fn relatorio_vazio_diz_tudo_fechado() {
        let texto = montar_relatorio(&cfg_ligado(), &EstadoDisjuntor::vazio(), 1_000);
        assert!(texto.contains("LIGADO"));
        assert!(texto.contains("Todos os circuitos fechados"));
    }

    #[test]
    fn relatorio_mostra_provedor_aberto() {
        let cfg = cfg_ligado();
        let mut estado = EstadoDisjuntor::vazio();
        for _ in 0..3 {
            estado.apos_falha("claude", 0, &cfg); // aberto até 60
        }
        // Em t=10 faltam 50s.
        let texto = montar_relatorio(&cfg, &estado, 10);
        assert!(texto.contains("🔴 claude: ABERTO"));
        assert!(texto.contains("reabre em"));
        assert!(texto.contains("3 falha"));
    }

    #[test]
    fn relatorio_avisa_quando_desligado() {
        let cfg = ConfigDisjuntor {
            habilitado: false,
            ..cfg_ligado()
        };
        let texto = montar_relatorio(&cfg, &EstadoDisjuntor::vazio(), 0);
        assert!(texto.contains("DESLIGADO na config"));
    }

    #[test]
    fn relatorio_mostra_provedor_fechado_com_falhas() {
        let cfg = cfg_ligado();
        let mut estado = EstadoDisjuntor::vazio();
        // 2 falhas (abaixo do limiar 3): conhecido, fechado, mas com falhas contadas.
        estado.apos_falha("gemini", 0, &cfg);
        estado.apos_falha("gemini", 0, &cfg);
        let texto = montar_relatorio(&cfg, &estado, 100);
        assert!(texto.contains("🟢 gemini: fechado"));
        assert!(texto.contains("2 falha"));
    }
}
