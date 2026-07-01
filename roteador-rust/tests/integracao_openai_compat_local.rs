//! Teste de integração do provedor `openai_compat` em modo LOCAL (http, sem chave).
//!
//! Prova o caminho novo ponta-a-ponta contra um servidor OpenAI-compatível de MENTIRA
//! (um `TcpListener` da stdlib que responde uma vez), sem depender de nenhum serviço
//! externo. Roda por PADRÃO (não é `#[ignore]`): é hermético — só loopback local, nada
//! de rede de verdade, e a ordem tem só o provedor local, então o Claude NUNCA é
//! disparado (respeita a regra de nunca acionar o refresh do token "só pra testar").

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use roteador::config::{Config, ConfigProvedor};
use roteador::{rotear, Contexto};

/// Lê a requisição HTTP inteira do socket: cabeçalhos até `\r\n\r\n` e, depois, o corpo
/// do tamanho anunciado em `Content-Length`. Devolve os bytes crus (cabeçalho + corpo).
/// Sem `unwrap` de I/O escondido: erro de leitura encerra a leitura devolvendo o que veio.
fn ler_requisicao(fluxo: &mut TcpStream) -> Vec<u8> {
    fluxo
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("definir timeout de leitura no mock");
    let mut dados = Vec::new();
    let mut pedaco = [0u8; 1024];
    loop {
        let lidos = match fluxo.read(&mut pedaco) {
            Ok(0) => break,  // socket fechado pela outra ponta
            Ok(n) => n,      // veio um pedaço
            Err(_) => break, // timeout ou erro: paramos com o que temos
        };
        dados.extend_from_slice(&pedaco[..lidos]);

        // Já temos o fim dos cabeçalhos? Então descobrimos o Content-Length e paramos
        // assim que o corpo estiver completo (não dá para depender de o cliente fechar
        // antes, pois ele espera a NOSSA resposta).
        if let Some(fim_cabecalho) = achar_fim_cabecalho(&dados) {
            let tamanho_corpo = content_length(&dados[..fim_cabecalho]).unwrap_or(0);
            if dados.len() >= fim_cabecalho + tamanho_corpo {
                break;
            }
        }
    }
    dados
}

/// Índice logo APÓS o `\r\n\r\n` que separa cabeçalhos do corpo (ou `None` se ainda não veio).
fn achar_fim_cabecalho(dados: &[u8]) -> Option<usize> {
    dados
        .windows(4)
        .position(|janela| janela == b"\r\n\r\n")
        .map(|inicio| inicio + 4)
}

/// Extrai o valor de `Content-Length` dos cabeçalhos (sem diferenciar maiúsculas).
fn content_length(cabecalhos: &[u8]) -> Option<usize> {
    let texto = String::from_utf8_lossy(cabecalhos);
    for linha in texto.lines() {
        if let Some((chave, valor)) = linha.split_once(':') {
            if chave.trim().eq_ignore_ascii_case("content-length") {
                return valor.trim().parse().ok();
            }
        }
    }
    None
}

#[test]
fn openai_compat_local_http_roteia_sem_chave() {
    // 1. Sobe o servidor de mentira numa porta livre (:0 = o SO escolhe uma).
    let ouvinte = TcpListener::bind("127.0.0.1:0").expect("bind do mock OpenAI-compat");
    let porta = ouvinte.local_addr().expect("porta do mock").port();

    // Numa thread, atende UMA conexão: lê o pedido, guarda-o e responde no formato OpenAI.
    let atendente = thread::spawn(move || {
        let (mut fluxo, _) = ouvinte.accept().expect("aceitar conexão no mock");
        let pedido = ler_requisicao(&mut fluxo);

        let corpo =
            r#"{"choices":[{"message":{"role":"assistant","content":"Olá do servidor local!"}}]}"#;
        let resposta = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            corpo.len(),
            corpo
        );
        fluxo
            .write_all(resposta.as_bytes())
            .expect("escrever resposta do mock");
        fluxo.flush().ok();
        // Devolve o pedido cru para a thread principal inspecionar (ex.: sem Authorization).
        String::from_utf8_lossy(&pedido).into_owned()
    });

    // 2. Config com só o provedor local (http, SEM chave). Sem Claude na ordem.
    let config = Config {
        ordem_fallback: vec!["local".to_string()],
        provedores: vec![ConfigProvedor {
            nome: "local".into(),
            tipo: "openai_compat".into(),
            url_base: Some(format!("http://127.0.0.1:{porta}/v1")),
            modelo: Some("modelo-local".into()),
            comando: None,
            chave: None, // <- sem chave: o caminho local não precisa
            mensagem_fixa: None,
            timeout: Duration::from_secs(5),
            habilitado: true,
            retentativas: 0,
            retentativa_espera_ms: 250,
        }],
        disjuntor: Default::default(),
        historico: Default::default(),
        telemetria_log: std::env::temp_dir()
            .join("roteador-integracao-openai-compat-local.log")
            .to_string_lossy()
            .to_string(),
        orcamento_total_ms: None,
    };

    // 3. Roteia de verdade pelo nosso cliente HTTP cru (sem curl/TLS, sem chave).
    let resposta = rotear("oi", &Contexto::vazio(), &config).expect("o mock deveria responder");

    assert_eq!(resposta.provedor, "local");
    assert_eq!(resposta.texto, "Olá do servidor local!");

    // 4. Confere no pedido cru que NÃO mandamos cabeçalho de autorização (não há chave) e
    //    que fomos ao endpoint certo `/v1/chat/completions`.
    let pedido = atendente.join().expect("thread do mock terminou");
    assert!(
        !pedido.to_lowercase().contains("authorization"),
        "sem chave, não deveria haver cabeçalho Authorization; pedido: {pedido}"
    );
    assert!(
        pedido.contains("POST /v1/chat/completions"),
        "deveria bater no endpoint OpenAI-compat; pedido: {pedido}"
    );
}
