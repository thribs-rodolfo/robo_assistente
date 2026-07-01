//! Binário que imprime as métricas agregadas de telemetria do roteador.
//!
//! Uso:
//!   metricas                       # lê o log padrão, agrega TUDO
//!   metricas /caminho/outro.log    # lê outro arquivo de log
//!   metricas --janela 24h          # só as últimas 24 horas (aceita 90m, 24h, 7d)
//!   metricas --janela 6h /tmp/x.log
//!   metricas --custo claude=3 --custo gemini=0.5   # estima custo por RESPOSTA por provedor
//!   metricas --custo-por-mil-tokens claude=2        # estima custo por TOKEN (~chars/4)
//!   metricas --json                # saída legível por máquina (dashboard/outro programa)
//!   metricas --json --janela 24h --custo claude=3   # combina com as demais opções
//!
//! Responde, de forma legível, "de quem o robô realmente depende?": por provedor,
//! quantas vezes respondeu/falhou/foi pulado e a latência média; quantas vezes caímos
//! no piso (Ollama); e a sequência de quedas seguidas no piso (alarme de dependência).
//! Com `--json`, o MESMO conteúdo sai como um objeto JSON (uma linha) para outra
//! ferramenta consumir. Só LÊ o log — nunca dispara provedor, então é seguro rodar à vontade.
//!
//! Saída de processo: 0 em sucesso, 1 se não conseguir ler o log ou se o argumento for inválido.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use roteador::duracao::{descrever_duracao, parsear_duracao};
use roteador::metricas;
use roteador::telemetria;

fn main() -> ExitCode {
    // Pula o nome do programa e interpreta os argumentos (ordem livre: --janela e caminho).
    let argumentos: Vec<String> = std::env::args().skip(1).collect();
    let opcoes = match interpretar_argumentos(&argumentos) {
        Ok(opcoes) => opcoes,
        Err(msg) => {
            eprintln!("[metricas] {msg}");
            return ExitCode::FAILURE;
        }
    };

    // Com janela, precisamos do "agora" do relógio do sistema para recortar o passado recente.
    let resultado = match opcoes.janela_segundos {
        Some(janela) => {
            let agora = match agora_epoch() {
                Some(t) => t,
                None => {
                    eprintln!(
                        "[metricas] relógio do sistema antes de 1970 — não dá pra usar janela"
                    );
                    return ExitCode::FAILURE;
                }
            };
            metricas::agregar_janela_de_arquivo(&opcoes.caminho, agora, janela)
        }
        None => metricas::agregar_de_arquivo(&opcoes.caminho),
    };

    match resultado {
        Ok(relatorio) => {
            // Modo máquina: um objeto JSON em uma linha, nada de texto humano em volta
            // (senão não seria JSON válido para quem consome). O bloco `custo` entra só se
            // houve `--custo`, igual ao relatório de texto.
            if opcoes.json {
                println!(
                    "{}",
                    relatorio
                        .para_json(&opcoes.precos, &opcoes.precos_por_token)
                        .para_texto()
                );
                return ExitCode::SUCCESS;
            }
            if let Some(janela) = opcoes.janela_segundos {
                println!("(janela: últimas {})", descrever_duracao(janela));
            }
            print!("{relatorio}");
            // Só imprime o custo por resposta se o operador passou pelo menos um `--custo`.
            if let Some(secao) = relatorio.secao_custo(&opcoes.precos) {
                print!("{secao}");
            }
            // Idem para o custo por token, com `--custo-por-mil-tokens`.
            if let Some(secao) = relatorio.secao_custo_por_token(&opcoes.precos_por_token) {
                print!("{secao}");
            }
            // Frescor: há quanto tempo cada provedor bom respondeu. Precisa do relógio real
            // ("há X"); se ele estiver quebrado (< 1970), pulamos só esta seção — o resto do
            // relatório e o `--json` (que traz epochs absolutos) seguem válidos.
            if let Some(agora) = agora_epoch() {
                if let Some(secao) = relatorio.secao_frescor(agora) {
                    print!("{secao}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(erro) => {
            eprintln!("[metricas] não consegui ler '{}': {erro}", opcoes.caminho);
            ExitCode::FAILURE
        }
    }
}

/// O que foi pedido na linha de comando.
struct Opcoes {
    /// Caminho do log a ler (default: o da telemetria).
    caminho: String,
    /// Janela em segundos, ou `None` para agregar tudo.
    janela_segundos: Option<u64>,
    /// Tabela de preços (provedor -> custo por resposta) vinda dos `--custo nome=valor`.
    /// Vazia quando não foi passado nenhum: aí o relatório não mostra custo.
    precos: BTreeMap<String, f64>,
    /// Tabela de preços (provedor -> custo por MIL tokens estimados) vinda dos
    /// `--custo-por-mil-tokens nome=valor`. Vazia = sem seção de custo por token.
    precos_por_token: BTreeMap<String, f64>,
    /// `true` quando `--json`: imprime o relatório como JSON (máquina) em vez de texto.
    json: bool,
}

/// Interpreta os argumentos: `--janela <dur>` (em qualquer posição) e, opcionalmente, um caminho.
/// Devolve `Err(mensagem)` em uso inválido — sem `panic`, sem erro silencioso.
fn interpretar_argumentos(args: &[String]) -> Result<Opcoes, String> {
    let mut caminho: Option<String> = None;
    let mut janela_segundos: Option<u64> = None;
    let mut precos: BTreeMap<String, f64> = BTreeMap::new();
    let mut precos_por_token: BTreeMap<String, f64> = BTreeMap::new();
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--json" {
            json = true;
            i += 1;
        } else if arg == "--janela" || arg == "-j" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um valor (ex.: 24h, 90m, 7d)"))?;
            janela_segundos = Some(parsear_duracao(valor)?);
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--janela=") {
            janela_segundos = Some(parsear_duracao(valor)?);
            i += 1;
        } else if arg == "--custo-por-mil-tokens" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um valor (ex.: claude=0.5)"))?;
            let (nome, preco) = parsear_preco(valor, "--custo-por-mil-tokens")?;
            precos_por_token.insert(nome, preco);
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--custo-por-mil-tokens=") {
            let (nome, preco) = parsear_preco(valor, "--custo-por-mil-tokens")?;
            precos_por_token.insert(nome, preco);
            i += 1;
        } else if arg == "--custo" {
            let valor = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg} precisa de um valor (ex.: claude=3)"))?;
            let (nome, preco) = parsear_preco(valor, "--custo")?;
            precos.insert(nome, preco);
            i += 2;
        } else if let Some(valor) = arg.strip_prefix("--custo=") {
            let (nome, preco) = parsear_preco(valor, "--custo")?;
            precos.insert(nome, preco);
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("opção desconhecida: {arg}"));
        } else if caminho.is_none() {
            caminho = Some(arg.clone());
            i += 1;
        } else {
            return Err(format!("argumento extra inesperado: {arg}"));
        }
    }

    Ok(Opcoes {
        caminho: caminho.unwrap_or_else(|| telemetria::ARQUIVO_LOG.to_string()),
        janela_segundos,
        precos,
        precos_por_token,
        json,
    })
}

/// Interpreta um `nome=valor` de um flag de preço (ex.: `claude=3.5`) em (nome, preço).
/// `flag` é o nome do flag só para a mensagem de erro (`--custo` ou `--custo-por-mil-tokens`),
/// já que os dois compartilham exatamente o mesmo formato e validação. O preço é sempre
/// não-negativo e finito, na unidade que o operador escolher.
/// Devolve `Err(mensagem)` em formato inválido — sem `panic`, sem erro silencioso.
fn parsear_preco(texto: &str, flag: &str) -> Result<(String, f64), String> {
    let (nome, valor) = texto
        .split_once('=')
        .ok_or_else(|| format!("{flag} espera nome=valor (ex.: claude=3), veio: '{texto}'"))?;
    let nome = nome.trim();
    if nome.is_empty() {
        return Err(format!("{flag} sem nome de provedor: '{texto}'"));
    }
    let preco: f64 = valor
        .trim()
        .parse()
        .map_err(|_| format!("{flag} com preço inválido: '{valor}' (use um número, ex.: 3.5)"))?;
    if !preco.is_finite() || preco < 0.0 {
        return Err(format!(
            "{flag} com preço inválido: '{valor}' (precisa ser >= 0)"
        ));
    }
    Ok((nome.to_string(), preco))
}

/// Instante atual em epoch (segundos UTC), ou `None` se o relógio estiver antes de 1970.
fn agora_epoch() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn interpreta_caminho_e_janela_em_qualquer_ordem() {
        let o =
            interpretar_argumentos(&["--janela".into(), "6h".into(), "/tmp/x.log".into()]).unwrap();
        assert_eq!(o.caminho, "/tmp/x.log");
        assert_eq!(o.janela_segundos, Some(21_600));

        let o2 = interpretar_argumentos(&["/tmp/y.log".into(), "--janela=2d".into()]).unwrap();
        assert_eq!(o2.caminho, "/tmp/y.log");
        assert_eq!(o2.janela_segundos, Some(172_800));
    }

    #[test]
    fn sem_argumentos_usa_log_padrao_e_tudo() {
        let o = interpretar_argumentos(&[]).unwrap();
        assert_eq!(o.caminho, telemetria::ARQUIVO_LOG);
        assert_eq!(o.janela_segundos, None);
        assert!(o.precos.is_empty());
        assert!(!o.json); // sem --json, saída é texto humano
    }

    #[test]
    fn flag_json_liga_saida_de_maquina_e_combina() {
        let o = interpretar_argumentos(&["--json".into()]).unwrap();
        assert!(o.json);

        // --json convive com --janela, caminho e --custo em qualquer ordem.
        let o2 = interpretar_argumentos(&[
            "--janela".into(),
            "24h".into(),
            "--json".into(),
            "/tmp/x.log".into(),
            "--custo".into(),
            "claude=3".into(),
        ])
        .unwrap();
        assert!(o2.json);
        assert_eq!(o2.janela_segundos, Some(86_400));
        assert_eq!(o2.caminho, "/tmp/x.log");
        assert_eq!(o2.precos.get("claude"), Some(&3.0));
    }

    #[test]
    fn erros_de_uso_viram_err() {
        assert!(interpretar_argumentos(&["--janela".into()]).is_err()); // sem valor
        assert!(interpretar_argumentos(&["--xpto".into()]).is_err()); // opção desconhecida
        assert!(interpretar_argumentos(&["a".into(), "b".into()]).is_err()); // dois caminhos
    }

    #[test]
    fn coleta_varios_precos_de_custo() {
        let o = interpretar_argumentos(&[
            "--custo".into(),
            "claude=3".into(),
            "--custo=gemini=0.5".into(),
        ])
        .unwrap();
        assert_eq!(o.precos.get("claude"), Some(&3.0));
        assert_eq!(o.precos.get("gemini"), Some(&0.5));
    }

    #[test]
    fn parsear_preco_valida_formato_e_sinal() {
        assert_eq!(
            parsear_preco("claude=3.5", "--custo").unwrap(),
            ("claude".to_string(), 3.5)
        );
        assert_eq!(
            parsear_preco(" groq = 0 ", "--custo").unwrap(),
            ("groq".to_string(), 0.0)
        );
        assert!(parsear_preco("semigual", "--custo").is_err()); // falta '='
        assert!(parsear_preco("=3", "--custo").is_err()); // sem nome
        assert!(parsear_preco("x=abc", "--custo").is_err()); // preço não-numérico
        assert!(parsear_preco("x=-1", "--custo").is_err()); // preço negativo
                                                            // A mensagem de erro cita o flag recebido (útil pro operador saber qual errou).
        let msg = parsear_preco("semigual", "--custo-por-mil-tokens").unwrap_err();
        assert!(msg.contains("--custo-por-mil-tokens"));
    }

    #[test]
    fn coleta_precos_por_mil_tokens_separado_do_custo_por_resposta() {
        let o = interpretar_argumentos(&[
            "--custo".into(),
            "claude=3".into(),
            "--custo-por-mil-tokens".into(),
            "claude=0.5".into(),
            "--custo-por-mil-tokens=gemini=0.25".into(),
        ])
        .unwrap();
        // As duas tabelas são independentes: preço por resposta ≠ preço por token.
        assert_eq!(o.precos.get("claude"), Some(&3.0));
        assert_eq!(o.precos_por_token.get("claude"), Some(&0.5));
        assert_eq!(o.precos_por_token.get("gemini"), Some(&0.25));
        assert!(!o.precos.contains_key("gemini"));
    }
}
