//! Escrita ATÔMICA de arquivo: troca o conteúdo "de uma vez só", sem nunca deixar o
//! arquivo pela metade para quem estiver lendo ao mesmo tempo.
//!
//! ## Por que isto existe (correção real, não enfeite)
//!
//! O estado do disjuntor ([`crate::disjuntor`]) é gravado pela ponte, que atende CADA
//! mensagem numa thread própria (`bin/ponte-telegram` faz `thread::spawn` por conexão).
//! Se duas mensagens chegam quase juntas, uma thread pode estar LENDO o estado enquanto
//! outra o reescreve. A escrita ingênua — abrir com `truncate` e depois `write_all` —
//! primeiro ZERA o arquivo e só então grava o novo conteúdo. Nessa janela, um leitor
//! concorrente (outra thread de roteamento, o `bin/disjuntor`, um cron) enxerga um arquivo
//! VAZIO ou pela METADE. O `carregar` do disjuntor trata isso como "corrompido → tudo
//! fechado", ou seja, ESQUECE os circuitos abertos justamente sob carga — o oposto do que
//! o disjuntor deveria fazer (evitar pagar a latência de um provedor morto a cada mensagem).
//!
//! ## A técnica
//!
//! A mesma "mv atômico" que já usamos para trocar o binário em produção, agora aplicada ao
//! estado: escreve num arquivo TEMPORÁRIO ao lado do destino e depois faz `rename` por cima.
//! No POSIX, `rename(2)` dentro do MESMO sistema de arquivos é atômico: um leitor sempre vê o
//! arquivo ANTIGO inteiro ou o NOVO inteiro, nunca um meio-termo. Zero dependências (`std::fs`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Grava `conteudo` em `caminho` de forma atômica: escreve num temporário vizinho e o
/// renomeia por cima do destino. Devolve `Err` se qualquer passo falhar — quem chama decide
/// se loga (é o padrão do projeto: erro é valor, nunca engolido em silêncio).
///
/// O temporário fica no MESMO diretório do destino de propósito: `rename` só é atômico
/// dentro do mesmo sistema de arquivos. Um temporário em `/tmp`, por exemplo, poderia cair
/// noutro sistema de arquivos e o `rename` falharia com `EXDEV` (cross-device).
pub fn escrever_atomico(caminho: &str, conteudo: &[u8]) -> std::io::Result<()> {
    let destino = Path::new(caminho);
    let temporario = caminho_temporario(destino);

    // 1) Escreve TODO o conteúdo no temporário. O escopo próprio garante que o arquivo é
    //    fechado (e o buffer descarregado) antes do rename lá embaixo.
    {
        let mut arquivo = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temporario)?;
        arquivo.write_all(conteudo)?;
        arquivo.flush()?;
    }

    // 2) Publica de uma vez: rename por cima do destino (atômico no POSIX, mesmo FS).
    match std::fs::rename(&temporario, destino) {
        Ok(()) => Ok(()),
        Err(erro) => {
            // Não conseguiu publicar. Tenta remover o temporário órfão para não deixar lixo —
            // mas sem MASCARAR o erro original: se até a limpeza falhar, avisa no stderr e
            // ainda assim propaga o erro do rename (que é o que de fato importa: não gravou).
            if let Err(erro_limpeza) = std::fs::remove_file(&temporario) {
                eprintln!(
                    "[arquivo] falha ao remover temporário {}: {erro_limpeza}",
                    temporario.display()
                );
            }
            Err(erro)
        }
    }
}

/// Monta um nome de temporário ÚNICO, ao lado do destino: `<destino>.tmp.<pid>.<thread>.<nanos>`.
///
/// A unicidade importa: se dois escritores gravam "ao mesmo tempo", cada um precisa do SEU
/// temporário — senão eles se atropelam no mesmo arquivo e o `rename` publicaria um conteúdo
/// misturado. Por isso combinamos o PID do processo, um resumo do id da thread e os
/// nanossegundos do relógio, o que torna colisão praticamente impossível na prática.
fn caminho_temporario(destino: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duracao| duracao.as_nanos())
        .unwrap_or(0);
    // O id da thread não tem `Display` estável; o `Debug` (ex.: "ThreadId(3)") serve só para
    // diferenciar threads. Guardamos apenas os caracteres alfanuméricos para um nome limpo.
    let thread_bruto = format!("{:?}", std::thread::current().id());
    let thread: String = thread_bruto
        .chars()
        .filter(|caractere| caractere.is_ascii_alphanumeric())
        .collect();

    // Anexa o sufixo ao caminho COMPLETO via `OsString`, preservando o nome original do
    // destino (funciona com qualquer nome de arquivo, sem supor extensão).
    let mut nome = destino.as_os_str().to_os_string();
    nome.push(format!(".tmp.{pid}.{thread}.{nanos}"));
    PathBuf::from(nome)
}

#[cfg(test)]
mod testes {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Caminho de teste único por processo, para não colidir entre execuções paralelas.
    fn caminho_de_teste(sufixo: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "roteador-arquivo-{}-{sufixo}.teste",
                std::process::id()
            ))
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn escreve_e_rele_o_conteudo() {
        let caminho = caminho_de_teste("basico");
        let _ = std::fs::remove_file(&caminho);

        escrever_atomico(&caminho, b"conteudo inicial").expect("deveria gravar");
        let lido = std::fs::read_to_string(&caminho).expect("deveria ler");
        assert_eq!(lido, "conteudo inicial");

        let _ = std::fs::remove_file(&caminho);
    }

    #[test]
    fn sobrescreve_conteudo_existente() {
        let caminho = caminho_de_teste("sobrescreve");
        let _ = std::fs::remove_file(&caminho);

        escrever_atomico(&caminho, b"primeiro valor bem longo").expect("gravar 1");
        escrever_atomico(&caminho, b"curto").expect("gravar 2");
        let lido = std::fs::read_to_string(&caminho).expect("ler");
        // Sem truncate atômico, sobra do valor longo poderia grudar no curto; aqui não.
        assert_eq!(lido, "curto");

        let _ = std::fs::remove_file(&caminho);
    }

    #[test]
    fn nao_deixa_temporario_orfao_apos_sucesso() {
        let caminho = caminho_de_teste("sem-orfao");
        let _ = std::fs::remove_file(&caminho);

        escrever_atomico(&caminho, b"ok").expect("gravar");

        // O diretório não deve conter nenhum resíduo `<destino>.tmp.*` deste destino.
        let destino = Path::new(&caminho);
        let diretorio = destino.parent().unwrap();
        let nome_destino = destino.file_name().unwrap().to_string_lossy().to_string();
        let prefixo_tmp = format!("{nome_destino}.tmp.");
        let residuos: Vec<_> = std::fs::read_dir(diretorio)
            .expect("ler dir")
            .filter_map(|entrada| entrada.ok())
            .map(|entrada| entrada.file_name().to_string_lossy().to_string())
            .filter(|nome| nome.starts_with(&prefixo_tmp))
            .collect();
        assert!(residuos.is_empty(), "temporários órfãos: {residuos:?}");

        let _ = std::fs::remove_file(&caminho);
    }

    /// O teste que PROVA a correção: enquanto escritores trocam entre um valor curto e um
    /// bem grande (maior que qualquer buffer de escrita), um leitor concorrente lê em loop e
    /// nunca pode ver um conteúdo "pela metade". Com a escrita ingênua (truncate + write) ele
    /// veria strings vazias/parciais; com a escrita atômica, sempre um dos dois valores inteiros.
    #[test]
    fn leitor_concorrente_nunca_ve_conteudo_pela_metade() {
        let caminho = caminho_de_teste("concorrente");
        let _ = std::fs::remove_file(&caminho);

        let curto = "a".repeat(16);
        let longo = "b".repeat(64 * 1024); // bem maior que um buffer típico de escrita
                                           // Valor inicial, para o leitor sempre encontrar o arquivo já existente.
        escrever_atomico(&caminho, curto.as_bytes()).expect("gravar inicial");

        let parar = Arc::new(AtomicBool::new(false));

        let leitor = {
            let caminho = caminho.clone();
            let parar = parar.clone();
            let curto = curto.clone();
            let longo = longo.clone();
            std::thread::spawn(move || {
                let mut leituras: u64 = 0;
                while !parar.load(Ordering::Relaxed) {
                    if let Ok(lido) = std::fs::read_to_string(&caminho) {
                        assert!(
                            lido == curto || lido == longo,
                            "leu conteúdo pela metade: {} bytes",
                            lido.len()
                        );
                        leituras += 1;
                    }
                }
                leituras
            })
        };

        for indice in 0..500u32 {
            let conteudo = if indice % 2 == 0 { &curto } else { &longo };
            escrever_atomico(&caminho, conteudo.as_bytes()).expect("gravar no loop");
        }
        parar.store(true, Ordering::Relaxed);

        // Se o leitor tivesse visto um conteúdo pela metade, seu `assert` teria entrado em
        // pânico e o `join` devolveria `Err` — então aqui EXIGIMOS que ele tenha terminado bem.
        let leituras = leitor
            .join()
            .expect("thread leitora entrou em pânico (leu conteúdo pela metade)");
        assert!(leituras > 0, "o leitor não conseguiu ler nada");

        let _ = std::fs::remove_file(&caminho);
    }
}
