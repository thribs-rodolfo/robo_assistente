//! Memória curta de conversa por chat — dá ao robô um histórico dos últimos turnos.
//!
//! ## Por que existe (fecha um buraco real)
//!
//! Sem isto, cada mensagem era roteada SEM contexto: o [`crate::prompt::Contexto`] ia sempre
//! com `historico` vazio, então o modelo respondia como se nunca tivesse falado com você.
//! Um "e o segundo?" ou "explica melhor" — que dependem do que veio ANTES — ficavam sem
//! sentido. Aqui guardamos os últimos turnos de cada chat em disco e os devolvemos como
//! histórico na próxima mensagem, dando "memória curta" ao assistente.
//!
//! ## Desenho (manifesto WORKSPACE_RULES "Como escrevemos código")
//!
//! - **Um arquivo por chat** (`<diretorio>/<chat>.json`): chats diferentes nunca disputam o
//!   mesmo arquivo. Para o caso raro de duas mensagens do MESMO chat chegarem quase juntas (a
//!   ponte atende cada update numa thread própria), a gravação é ATÔMICA
//!   ([`crate::arquivo::escrever_atomico`]) — um leitor nunca vê o arquivo pela metade.
//! - **Limitado**: guardamos só os últimos `max_turnos` turnos e truncamos cada turno em
//!   `max_chars_por_turno`. Memória curta não pode crescer sem teto — nem no disco, nem no
//!   tamanho do prompt (que vira custo/latência no provedor).
//! - **Degrada com graça**: arquivo ausente = conversa nova (histórico vazio, normal, sem
//!   ruído). Arquivo corrompido = histórico vazio + aviso no `stderr` (nunca engolido em
//!   silêncio, nunca propagado ao usuário). A memória é um EXTRA: jamais deve impedir uma
//!   resposta.
//! - **Núcleo puro**: `podar`, `truncar`, `com_nova_troca`, `serializar`/`desserializar` são
//!   funções puras e testáveis sem disco; só `carregar`/`salvar`/`registrar_troca` tocam o disco.
//! - **Opt-in**: controlado por [`crate::config::ConfigHistorico`], DESLIGADO por padrão. Quando
//!   desligado, a ponte nem chama este módulo → comportamento idêntico ao de antes (risco zero).

use crate::arquivo;
use crate::config::ConfigHistorico;
use crate::json::{self, Valor};
use crate::prompt::{Autor, Turno};

/// Erro ao persistir o histórico. Valor tipado — quem chama loga, mas NUNCA deixa isto
/// derrubar a resposta ao usuário (a memória é um extra).
#[derive(Debug, Clone, PartialEq)]
pub enum ErroHistorico {
    /// Falha ao criar o diretório ou gravar o arquivo do chat.
    Escrita(String),
}

impl std::fmt::Display for ErroHistorico {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErroHistorico::Escrita(motivo) => write!(f, "não gravei o histórico: {motivo}"),
        }
    }
}

impl std::error::Error for ErroHistorico {}

/// Caminho do arquivo de histórico de um chat: `<diretorio>/<chat>.json`.
///
/// O `chat` é um inteiro do Telegram (pode ser negativo em grupos), então o nome de arquivo é
/// sempre `[-]dígitos.json` — sem barra nem caractere perigoso, imune a "path traversal".
pub fn caminho_do_chat(diretorio: &str, chat: i64) -> String {
    // Tira uma eventual barra final do diretório para não gerar `dir//123.json`.
    let base = diretorio.trim_end_matches('/');
    format!("{base}/{chat}.json")
}

/// Carrega os turnos guardados de um chat, já PODADOS ao teto `max_turnos`.
///
/// Arquivo ausente => conversa nova, histórico vazio (silêncio proposital: é o caso comum).
/// Arquivo ilegível/corrompido => histórico vazio + aviso no `stderr` (degrada com graça sem
/// engolir o erro). Nunca devolve `Err`: a memória é um extra e não pode impedir a resposta.
pub fn carregar(config: &ConfigHistorico, chat: i64) -> Vec<Turno> {
    let caminho = caminho_do_chat(&config.diretorio, chat);
    let conteudo = match std::fs::read_to_string(&caminho) {
        Ok(texto) => texto,
        Err(erro) if erro.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(erro) => {
            eprintln!("[historico] não li '{caminho}': {erro} — seguindo sem memória");
            return Vec::new();
        }
    };
    match desserializar(&conteudo) {
        Some(turnos) => podar(turnos, config.max_turnos),
        None => {
            eprintln!("[historico] '{caminho}' corrompido — seguindo sem memória");
            Vec::new()
        }
    }
}

/// Grava a troca recém-ocorrida (mensagem do usuário + resposta do assistente) no histórico
/// do chat, PODANDO ao teto e truncando cada turno. Recebe o `historico_anterior` (o que foi
/// usado nesta resposta) para anexar em cima, evitando reler o arquivo.
///
/// Devolve `Err` se a gravação falhar — quem chama (a ponte) LOGA, mas segue: a resposta ao
/// usuário já foi enviada; perder a memória de um turno é degradação aceitável, não erro fatal.
pub fn registrar_troca(
    config: &ConfigHistorico,
    chat: i64,
    historico_anterior: &[Turno],
    mensagem_usuario: &str,
    resposta_assistente: &str,
) -> Result<(), ErroHistorico> {
    let novos = com_nova_troca(
        historico_anterior,
        mensagem_usuario,
        resposta_assistente,
        config.max_turnos,
        config.max_chars_por_turno,
    );
    salvar(config, chat, &novos)
}

/// Persiste uma lista de turnos (já pronta) no arquivo do chat, de forma atômica.
///
/// Cria o diretório se faltar. Separado de [`registrar_troca`] para ser reusável e testável.
pub fn salvar(config: &ConfigHistorico, chat: i64, turnos: &[Turno]) -> Result<(), ErroHistorico> {
    // Garante o diretório (a escrita atômica põe o temporário AO LADO do destino, então o
    // diretório precisa existir antes). `create_dir_all` é idempotente (ok se já existe).
    if let Err(erro) = std::fs::create_dir_all(config.diretorio.trim_end_matches('/')) {
        return Err(ErroHistorico::Escrita(format!(
            "criar diretório '{}': {erro}",
            config.diretorio
        )));
    }
    let caminho = caminho_do_chat(&config.diretorio, chat);
    let conteudo = serializar(turnos);
    arquivo::escrever_atomico(&caminho, conteudo.as_bytes())
        .map_err(|erro| ErroHistorico::Escrita(format!("gravar '{caminho}': {erro}")))
}

/// Monta o histórico NOVO a partir do anterior + a troca atual (usuário e assistente),
/// já truncando cada texto e podando ao teto. Função PURA (sem disco), fácil de testar.
///
/// A ordem importa: a mensagem do usuário vem ANTES da resposta do assistente (é a sequência
/// real da conversa). A poda mantém os últimos `max_turnos` — o que preserva os turnos mais
/// RECENTES (a poda descarta os mais antigos, que é o comportamento certo para memória curta).
pub fn com_nova_troca(
    anterior: &[Turno],
    mensagem_usuario: &str,
    resposta_assistente: &str,
    max_turnos: usize,
    max_chars_por_turno: usize,
) -> Vec<Turno> {
    let mut turnos: Vec<Turno> = anterior.to_vec();
    turnos.push(Turno {
        autor: Autor::Usuario,
        texto: truncar(mensagem_usuario, max_chars_por_turno),
    });
    turnos.push(Turno {
        autor: Autor::Assistente,
        texto: truncar(resposta_assistente, max_chars_por_turno),
    });
    podar(turnos, max_turnos)
}

/// Mantém apenas os ÚLTIMOS `max_turnos` turnos (descarta os mais antigos). Função pura.
///
/// `max_turnos == 0` desativa na prática (nada é guardado). Preservamos os recentes porque
/// memória curta quer o contexto imediato, não o começo esquecido da conversa.
pub fn podar(turnos: Vec<Turno>, max_turnos: usize) -> Vec<Turno> {
    if turnos.len() <= max_turnos {
        return turnos;
    }
    let descartar = turnos.len() - max_turnos;
    turnos.into_iter().skip(descartar).collect()
}

/// Trunca um texto em `max_chars` CARACTERES (não bytes — respeita acento/emoji), anexando
/// um marcador "…" quando corta, para o modelo saber que houve corte. Função pura.
pub fn truncar(texto: &str, max_chars: usize) -> String {
    if texto.chars().count() <= max_chars {
        return texto.to_string();
    }
    // Deixa espaço para o marcador dentro do teto (se o teto for minúsculo, degrada só cortando).
    let manter = max_chars.saturating_sub(1).max(1);
    let cortado: String = texto.chars().take(manter).collect();
    format!("{cortado}…")
}

/// Serializa os turnos para o JSON de armazenamento (usando o nosso codificador, zero deps):
/// `{"turnos":[{"autor":"usuario","texto":"..."},{"autor":"assistente","texto":"..."}]}`.
pub fn serializar(turnos: &[Turno]) -> String {
    let lista: Vec<Valor> = turnos
        .iter()
        .map(|turno| {
            Valor::Objeto(vec![
                (
                    "autor".to_string(),
                    Valor::Texto(rotulo_autor(&turno.autor).to_string()),
                ),
                ("texto".to_string(), Valor::Texto(turno.texto.clone())),
            ])
        })
        .collect();
    Valor::Objeto(vec![("turnos".to_string(), Valor::Lista(lista))]).para_texto()
}

/// Interpreta o JSON de armazenamento de volta em turnos. Tolerante: turnos com autor
/// desconhecido ou sem texto são PULADOS (não derrubam o resto). Devolve `None` só quando o
/// JSON em si é inválido ou não tem a lista `turnos` — aí o chamador trata como "sem memória".
pub fn desserializar(texto_json: &str) -> Option<Vec<Turno>> {
    let raiz = json::parsear(texto_json).ok()?;
    let lista = raiz.obter("turnos").and_then(Valor::como_lista)?;
    let mut turnos = Vec::new();
    for item in lista {
        let autor = match item.obter("autor").and_then(Valor::como_texto) {
            Some("usuario") => Autor::Usuario,
            Some("assistente") => Autor::Assistente,
            _ => continue, // autor ausente/desconhecido: pula este turno, mantém os demais.
        };
        let texto = match item.obter("texto").and_then(Valor::como_texto) {
            Some(t) => t.to_string(),
            None => continue,
        };
        turnos.push(Turno { autor, texto });
    }
    Some(turnos)
}

/// Rótulo textual do autor no armazenamento. Casado com [`desserializar`].
fn rotulo_autor(autor: &Autor) -> &'static str {
    match autor {
        Autor::Usuario => "usuario",
        Autor::Assistente => "assistente",
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    fn config_de_teste(diretorio: &str) -> ConfigHistorico {
        ConfigHistorico {
            habilitado: true,
            diretorio: diretorio.to_string(),
            max_turnos: 4,
            max_chars_por_turno: 50,
        }
    }

    fn turno(autor: Autor, texto: &str) -> Turno {
        Turno {
            autor,
            texto: texto.to_string(),
        }
    }

    #[test]
    fn caminho_usa_chat_como_nome() {
        assert_eq!(caminho_do_chat("/var/hist", 123), "/var/hist/123.json");
        // Barra final não duplica; chat negativo (grupo) vira nome válido.
        assert_eq!(caminho_do_chat("/var/hist/", -100), "/var/hist/-100.json");
    }

    #[test]
    fn poda_mantem_os_turnos_mais_recentes() {
        let turnos = vec![
            turno(Autor::Usuario, "1"),
            turno(Autor::Assistente, "2"),
            turno(Autor::Usuario, "3"),
            turno(Autor::Assistente, "4"),
        ];
        let podados = podar(turnos, 2);
        assert_eq!(podados.len(), 2);
        assert_eq!(podados[0].texto, "3"); // os antigos (1,2) saíram
        assert_eq!(podados[1].texto, "4");
    }

    #[test]
    fn poda_nao_mexe_quando_cabe() {
        let turnos = vec![turno(Autor::Usuario, "a")];
        assert_eq!(podar(turnos.clone(), 5), turnos);
    }

    #[test]
    fn truncar_encurta_e_marca_o_corte() {
        let cortado = truncar("abcdefghij", 5);
        assert_eq!(cortado.chars().count(), 5);
        assert!(cortado.ends_with('…'));
        assert!(cortado.starts_with("abcd"));
    }

    #[test]
    fn truncar_nao_mexe_quando_cabe() {
        assert_eq!(truncar("curto", 50), "curto");
    }

    #[test]
    fn truncar_respeita_fronteira_de_char_multibyte() {
        // Só caracteres multibyte: o corte não pode partir um char no meio.
        let texto = "áéíóúàèìòù"; // 10 chars
        let cortado = truncar(texto, 4);
        assert_eq!(cortado.chars().count(), 4);
        assert!(cortado.ends_with('…'));
    }

    #[test]
    fn nova_troca_anexa_usuario_e_assistente_na_ordem() {
        let anterior = vec![turno(Autor::Usuario, "oi"), turno(Autor::Assistente, "olá")];
        let novos = com_nova_troca(&anterior, "tudo bem?", "tudo!", 10, 50);
        assert_eq!(novos.len(), 4);
        assert_eq!(novos[2].autor, Autor::Usuario);
        assert_eq!(novos[2].texto, "tudo bem?");
        assert_eq!(novos[3].autor, Autor::Assistente);
        assert_eq!(novos[3].texto, "tudo!");
    }

    #[test]
    fn nova_troca_poda_ao_teto() {
        // Teto 2: guarda só a troca atual (usuário+assistente), descartando o anterior.
        let anterior = vec![
            turno(Autor::Usuario, "velho"),
            turno(Autor::Assistente, "antigo"),
        ];
        let novos = com_nova_troca(&anterior, "u", "a", 2, 50);
        assert_eq!(novos.len(), 2);
        assert_eq!(novos[0].texto, "u");
        assert_eq!(novos[1].texto, "a");
    }

    #[test]
    fn nova_troca_trunca_textos_longos() {
        let longa = "x".repeat(100);
        let novos = com_nova_troca(&[], &longa, &longa, 10, 20);
        assert!(novos[0].texto.chars().count() <= 20);
        assert!(novos[0].texto.ends_with('…'));
    }

    #[test]
    fn serializa_e_desserializa_ida_e_volta() {
        let turnos = vec![
            turno(Autor::Usuario, "com \"aspas\" e\nquebra"),
            turno(Autor::Assistente, "resposta"),
        ];
        let json = serializar(&turnos);
        let de_volta = desserializar(&json).expect("deveria parsear");
        assert_eq!(de_volta, turnos);
    }

    #[test]
    fn desserializa_json_invalido_da_none() {
        assert_eq!(desserializar("isto não é json {"), None);
        // JSON válido mas sem a lista 'turnos' também é "sem memória".
        assert_eq!(desserializar(r#"{"outra_coisa":1}"#), None);
    }

    #[test]
    fn desserializa_pula_turno_com_autor_desconhecido() {
        // Um turno com autor inválido não derruba os demais (tolerância).
        let json = r#"{"turnos":[
            {"autor":"marciano","texto":"ignore"},
            {"autor":"usuario","texto":"válido"}
        ]}"#;
        let turnos = desserializar(json).unwrap();
        assert_eq!(turnos.len(), 1);
        assert_eq!(turnos[0].texto, "válido");
    }

    #[test]
    fn salvar_e_carregar_fazem_ida_e_volta_no_disco() {
        let dir = std::env::temp_dir()
            .join(format!("roteador-hist-teste-{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        let _ = std::fs::remove_dir_all(&dir);
        let config = config_de_teste(&dir);

        // Chat sem arquivo ainda => histórico vazio (conversa nova).
        assert!(carregar(&config, 42).is_empty());

        // Registra uma troca e relê.
        registrar_troca(&config, 42, &[], "oi", "olá").expect("gravar");
        let lido = carregar(&config, 42);
        assert_eq!(lido.len(), 2);
        assert_eq!(lido[0].texto, "oi");
        assert_eq!(lido[1].texto, "olá");

        // Uma segunda troca acumula em cima (memória curta funcionando).
        registrar_troca(&config, 42, &lido, "e agora?", "agora sim").expect("gravar 2");
        let lido2 = carregar(&config, 42);
        assert_eq!(lido2.len(), 4);
        assert_eq!(lido2[3].texto, "agora sim");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn carregar_poda_ao_teto_da_config() {
        let dir = std::env::temp_dir()
            .join(format!("roteador-hist-poda-{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = config_de_teste(&dir);
        config.max_turnos = 2;

        // Grava 4 turnos crus direto (bypassa a poda de gravação) e prova que a LEITURA poda.
        let quatro = vec![
            turno(Autor::Usuario, "1"),
            turno(Autor::Assistente, "2"),
            turno(Autor::Usuario, "3"),
            turno(Autor::Assistente, "4"),
        ];
        salvar(&config_de_teste(&dir), 7, &quatro).expect("gravar 4");
        let lido = carregar(&config, 7);
        assert_eq!(lido.len(), 2, "a leitura deve podar ao max_turnos vigente");
        assert_eq!(lido[0].texto, "3");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn carregar_arquivo_corrompido_devolve_vazio() {
        let dir = std::env::temp_dir()
            .join(format!("roteador-hist-corrompido-{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let caminho = caminho_do_chat(&dir, 9);
        std::fs::write(&caminho, "{ lixo não-json").unwrap();

        // Corrompido => vazio (degrada com graça), nunca pânico.
        assert!(carregar(&config_de_teste(&dir), 9).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
