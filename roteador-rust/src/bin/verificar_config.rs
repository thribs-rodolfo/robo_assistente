//! Binário que verifica a configuração do roteador ANTES de ela ir para o ar.
//!
//! Uso:
//!   verificar-config                        # verifica o arquivo padrão (/root/.secrets/...)
//!   verificar-config /tmp/outra-config.json # verifica outro arquivo
//!
//! Carrega a config, roda a verificação estática ([`roteador::verificacao`]) e imprime os
//! achados (erros e avisos) de forma legível. NUNCA constrói provedor para valer nem abre
//! rede — é 100% seguro rodar à vontade (jamais toca o Claude: licao-refresh-token-rotativo).
//!
//! Saída de processo:
//!   0  config sem ERROS (pode ter avisos)
//!   1  config com pelo menos um ERRO, ou falha ao ler/parsear o arquivo, ou uso inválido.
//! Isso deixa o binário útil em cron/CI: `verificar-config && deploy`.

use std::process::ExitCode;

use roteador::config;
use roteador::verificacao;

fn main() -> ExitCode {
    let argumentos: Vec<String> = std::env::args().skip(1).collect();
    let caminho = match interpretar_argumentos(&argumentos) {
        Ok(caminho) => caminho,
        Err(msg) => {
            eprintln!("[verificar-config] {msg}");
            return ExitCode::FAILURE;
        }
    };

    // Carrega e parseia. Erro de leitura/JSON já é uma falha da config → processo falha.
    let config = match config::carregar_de_arquivo(&caminho) {
        Ok(config) => config,
        Err(erro) => {
            eprintln!("[verificar-config] não carreguei '{caminho}': {erro}");
            return ExitCode::FAILURE;
        }
    };

    // Cabeçalho: mostra o que foi lido para o operador conferir de bater o olho.
    println!("Config: {caminho}");
    println!("Ordem de fallback: {}", config.ordem_fallback.join(" → "));
    let nomes: Vec<&str> = config.provedores.iter().map(|p| p.nome.as_str()).collect();
    println!(
        "Provedores declarados: {} ({})",
        nomes.len(),
        nomes.join(", ")
    );
    println!(
        "Disjuntor: {}",
        if config.disjuntor.habilitado {
            "ligado"
        } else {
            "desligado"
        }
    );
    println!();

    // Verificação pura + relatório.
    let achados = verificacao::verificar(&config);
    if achados.is_empty() {
        println!("✅ Tudo certo: nenhum problema encontrado na configuração.");
        return ExitCode::SUCCESS;
    }

    for achado in &achados {
        println!("{achado}");
    }
    let (erros, avisos) = verificacao::contar(&achados);
    println!();
    println!(
        "Resumo: {erros} {} e {avisos} {}.",
        se_plural(erros, "erro", "erros"),
        se_plural(avisos, "aviso", "avisos")
    );

    // Só ERRO derruba o código de saída; avisos passam (config funciona, mas convém olhar).
    if verificacao::tem_erro(&achados) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Interpreta os argumentos: um caminho opcional (default: [`config::CAMINHO_PADRAO`]).
/// Devolve `Err(mensagem)` em uso inválido — sem `panic`, sem erro silencioso.
fn interpretar_argumentos(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok(config::CAMINHO_PADRAO.to_string()),
        [caminho] if !caminho.starts_with('-') => Ok(caminho.clone()),
        [outro] => Err(format!("opção desconhecida: {outro}")),
        _ => Err("uso: verificar-config [caminho-da-config]".to_string()),
    }
}

/// Escolhe singular/plural conforme a contagem (relatório legível em pt-BR).
fn se_plural<'a>(n: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 {
        singular
    } else {
        plural
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn sem_argumento_usa_caminho_padrao() {
        assert_eq!(interpretar_argumentos(&[]).unwrap(), config::CAMINHO_PADRAO);
    }

    #[test]
    fn aceita_um_caminho() {
        assert_eq!(
            interpretar_argumentos(&["/tmp/x.json".into()]).unwrap(),
            "/tmp/x.json"
        );
    }

    #[test]
    fn rejeita_opcao_e_argumentos_extras() {
        assert!(interpretar_argumentos(&["--xpto".into()]).is_err());
        assert!(interpretar_argumentos(&["a".into(), "b".into()]).is_err());
    }

    #[test]
    fn plural_conta_certo() {
        assert_eq!(se_plural(1, "erro", "erros"), "erro");
        assert_eq!(se_plural(0, "erro", "erros"), "erros");
        assert_eq!(se_plural(2, "erro", "erros"), "erros");
    }
}
