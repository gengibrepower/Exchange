//! Servidor TCP da borda (seção 4.4). O TCP entrega um fluxo de bytes sem fronteiras; a
//! fronteira entre mensagens é convenção nossa: uma por linha, com teto de tamanho.

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use exchange_core::domain::AccountId;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::core_thread::{CoreError, CoreHandle};
use crate::market::{ASSETS, HOUSE};
use crate::protocol::{self, AssetBalance, ClientMessage, Inbound, ServerMessage};

/// Linha maior que isso sem `\n` não é protocolo: é erro e a conexão fecha, para um
/// cliente não fazer o servidor acumular memória sem fim (seção 6).
pub const MAX_LINE: usize = 4096;

/// Defesas contra exaustão de recursos (seção 6), além do teto de linha.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Sessões simultâneas; acima disso a conexão recebe "servidor cheio" e fecha.
    pub max_connections: NonZeroUsize,
    /// Tempo máximo para uma linha inteira chegar. Vale por linha, não por byte: um cliente
    /// que pinga um byte de vez em quando também é encerrado.
    pub idle_timeout: Duration,
}

/// O que fazer quando o `accept` falha.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcceptFailure {
    /// O problema era daquela conexão; aceita a próxima.
    Skip,
    /// Falta recurso agora; tentar de novo na hora só giraria a CPU no mesmo erro.
    Pause(Duration),
    /// O socket de escuta quebrou; o servidor para (fail-stop).
    Fatal,
}

pub const RESOURCE_PAUSE: Duration = Duration::from_millis(100);

// Códigos de erro do Linux que o `std` ainda não expõe como `io::ErrorKind` próprio.
const ENOMEM: i32 = 12;
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
const ENOBUFS: i32 = 105;

pub fn classify(error: &io::Error) -> AcceptFailure {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
    ) {
        return AcceptFailure::Skip;
    }
    match error.raw_os_error() {
        Some(ENOMEM | ENFILE | EMFILE | ENOBUFS) => AcceptFailure::Pause(RESOURCE_PAUSE),
        _ => AcceptFailure::Fatal,
    }
}

/// Aceita conexões para sempre; cada uma vira uma task com sua cópia do handle do núcleo e
/// uma vaga do limite de conexões, devolvida quando a sessão termina.
///
/// # Errors
/// Se o `accept` falhar de um jeito que [`classify`] considera fatal.
pub async fn serve(listener: TcpListener, core: CoreHandle, limits: Limits) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(
        limits.max_connections.get().min(Semaphore::MAX_PERMITS),
    ));
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => match classify(&error) {
                AcceptFailure::Skip => continue,
                AcceptFailure::Pause(pause) => {
                    tokio::time::sleep(pause).await;
                    continue;
                }
                AcceptFailure::Fatal => return Err(error),
            },
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tokio::spawn(refuse(stream));
            continue;
        };
        let core = core.clone();
        tokio::spawn(async move {
            session(stream, core, limits.idle_timeout).await;
            drop(slot);
        });
    }
}

async fn refuse(mut stream: TcpStream) {
    // Quem não coube só precisa saber o motivo; se nem isso chegar, não há o que fazer.
    let _ = stream
        .write_all(error("servidor cheio").to_line().as_bytes())
        .await;
}

/// Estado da sessão: antes do login só `login` é aceito; depois, tudo menos outro login,
/// sempre em nome da conta autenticada.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Session {
    AwaitingLogin,
    Active(AccountId),
}

/// Trata uma mensagem conforme o estado da sessão, possivelmente mudando o estado, e
/// devolve as respostas na ordem em que devem ser escritas.
///
/// # Errors
/// [`CoreError::Stopped`] se o núcleo parou: a sessão deve ser encerrada.
pub async fn respond(
    state: &mut Session,
    message: ClientMessage,
    core: &CoreHandle,
) -> Result<Vec<ServerMessage>, CoreError> {
    let Session::Active(account) = *state else {
        let ClientMessage::Login { account } = message else {
            return Ok(vec![error("faça login primeiro")]);
        };
        if account == HOUSE {
            return Ok(vec![error("a conta da casa não negocia")]);
        }
        *state = Session::Active(account);
        return Ok(vec![ServerMessage::LoggedIn { account }]);
    };
    match message.resolve(account) {
        Err(problem) => Ok(vec![error(problem.to_string())]),
        Ok(Inbound::Login(_)) => Ok(vec![error(format!(
            "sessão já autenticada como conta {}",
            account.get()
        ))]),
        Ok(Inbound::Balance) => Ok(vec![balances(core, account).await?]),
        Ok(Inbound::Submit(intent)) => Ok(core
            .submit(intent)
            .await?
            .iter()
            .map(ServerMessage::from)
            .collect()),
    }
}

fn error(message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        message: message.into(),
    }
}

async fn balances(core: &CoreHandle, account: AccountId) -> Result<ServerMessage, CoreError> {
    let mut balances = Vec::with_capacity(ASSETS.len());
    for (name, asset) in ASSETS {
        let balance = core.balance(account, asset).await?;
        balances.push(AssetBalance {
            asset: name,
            available: balance.available,
            reserved: balance.reserved,
        });
    }
    Ok(ServerMessage::Balances { balances })
}

async fn session(stream: TcpStream, core: CoreHandle, idle_timeout: Duration) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut state = Session::AwaitingLogin;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = tokio::time::timeout(idle_timeout, read_line(&mut reader, &mut line)).await;
        let (replies, keep_open) = match read {
            Err(_idle) => (vec![error("sessão ociosa")], false),
            Ok(Err(_) | Ok(Read::Closed)) => return,
            Ok(Ok(Read::TooLong)) => (
                vec![error(format!("linha passa de {MAX_LINE} bytes"))],
                false,
            ),
            Ok(Ok(Read::Line)) => handle(&line, &mut state, &core).await,
        };
        if write(&mut writer, &replies).await.is_err() || !keep_open {
            return;
        }
    }
}

async fn handle(line: &[u8], state: &mut Session, core: &CoreHandle) -> (Vec<ServerMessage>, bool) {
    let Ok(text) = std::str::from_utf8(line) else {
        return (vec![error("linha não é UTF-8")], true);
    };
    if text.trim().is_empty() {
        return (Vec::new(), true);
    }
    let message = match protocol::decode(text) {
        Ok(message) => message,
        Err(problem) => return (vec![error(problem.to_string())], true),
    };
    match respond(state, message, core).await {
        Ok(replies) => (replies, true),
        Err(stopped) => (vec![error(stopped.to_string())], false),
    }
}

enum Read {
    Line,
    Closed,
    TooLong,
}

/// Lê até `\n` sem nunca guardar mais que `MAX_LINE + 1` bytes. A linha sai sem o `\n` (e
/// sem um `\r` antes dele, para clientes que mandam CRLF).
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> io::Result<Read> {
    let limit = u64::try_from(MAX_LINE + 1).expect("teto pequeno cabe em u64");
    let read = reader.take(limit).read_until(b'\n', line).await?;
    if read == 0 {
        return Ok(Read::Closed);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        return Ok(Read::Line);
    }
    if line.len() > MAX_LINE {
        return Ok(Read::TooLong);
    }
    Ok(Read::Line)
}

async fn write(
    writer: &mut (impl AsyncWrite + Unpin),
    replies: &[ServerMessage],
) -> io::Result<()> {
    let bytes: String = replies.iter().map(ServerMessage::to_line).collect();
    writer.write_all(bytes.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

    use super::*;
    use crate::core_thread::spawn_core;
    use crate::market::new_market_data;
    use crate::sequencer::Sequencer;

    const PATIENCE: Duration = Duration::from_secs(5);

    const ROOMY: Limits = Limits {
        max_connections: NonZeroUsize::new(64).unwrap(),
        idle_timeout: PATIENCE,
    };

    async fn start(sequencer: Sequencer) -> SocketAddr {
        start_with(sequencer, ROOMY).await
    }

    async fn start_with(sequencer: Sequencer, limits: Limits) -> SocketAddr {
        let (core, _thread) = spawn_core(
            sequencer,
            new_market_data(),
            NonZeroUsize::new(16).unwrap(),
            NonZeroUsize::new(1024).unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, core, limits));
        address
    }

    struct Client {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
    }

    impl Client {
        async fn connect(address: SocketAddr) -> Self {
            let (reader, writer) = TcpStream::connect(address).await.unwrap().into_split();
            Self {
                reader: BufReader::new(reader),
                writer,
            }
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.writer.write_all(bytes).await.unwrap();
        }

        async fn send(&mut self, line: &str) {
            self.send_raw(format!("{line}\n").as_bytes()).await;
        }

        async fn recv(&mut self) -> String {
            let mut line = String::new();
            let read = tokio::time::timeout(PATIENCE, self.reader.read_line(&mut line))
                .await
                .expect("o servidor respondeu a tempo")
                .unwrap();
            assert!(read > 0, "o servidor fechou a conexão");
            line.trim_end_matches('\n').to_string()
        }

        async fn closed(&mut self) -> bool {
            let mut rest = Vec::new();
            let read = tokio::time::timeout(PATIENCE, self.reader.read_to_end(&mut rest))
                .await
                .expect("o servidor fechou a tempo");
            read.is_ok() && rest.is_empty()
        }

        async fn login(address: SocketAddr, account: u64) -> Self {
            let mut client = Self::connect(address).await;
            client
                .send(&format!(r#"{{"type":"login","account":{account}}}"#))
                .await;
            assert_eq!(
                client.recv().await,
                format!(r#"{{"type":"logged_in","account":{account}}}"#)
            );
            client
        }

        async fn fund(&mut self) {
            self.send(r#"{"type":"deposit","asset":"BRL","amount":100000000}"#)
                .await;
            self.recv().await;
            self.send(r#"{"type":"deposit","asset":"TESTE","amount":100000}"#)
                .await;
            self.recv().await;
        }
    }

    // O critério da F4: cliente conecta por TCP, manda ordem JSON, recebe resposta.
    #[tokio::test]
    async fn clients_trade_over_tcp() {
        let address = start(Sequencer::in_memory()).await;
        let mut alice = Client::login(address, 1).await;
        let mut bob = Client::login(address, 2).await;
        alice.fund().await;
        bob.fund().await;

        alice
            .send(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":5}"#,
            )
            .await;
        assert_eq!(
            alice.recv().await,
            r#"{"type":"rested","instrument":"TESTE/BRL","order":1,"lots":5}"#
        );

        bob.send(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":2}"#)
            .await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"fill","instrument":"TESTE/BRL","taker":2,"maker":1,"lots":2,"price":10000,"fee":20}"#
        );

        bob.send(r#"{"type":"balance"}"#).await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"balances","balances":[{"asset":"TESTE","available":100200,"reserved":0},{"asset":"BRL","available":99979980,"reserved":0}]}"#
        );
    }

    #[tokio::test]
    async fn nothing_but_login_is_accepted_before_login() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::connect(address).await;

        client.send(r#"{"type":"balance"}"#).await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"faça login primeiro"}"#
        );
        client
            .send(r#"{"type":"deposit","asset":"BRL","amount":1}"#)
            .await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"faça login primeiro"}"#
        );
    }

    #[tokio::test]
    async fn house_account_cannot_log_in() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::connect(address).await;

        client
            .send(&format!(r#"{{"type":"login","account":{}}}"#, HOUSE.get()))
            .await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"a conta da casa não negocia"}"#
        );
        client.send(r#"{"type":"balance"}"#).await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"faça login primeiro"}"#
        );
    }

    #[tokio::test]
    async fn second_login_is_refused_and_the_session_keeps_its_account() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::login(address, 1).await;

        client.send(r#"{"type":"login","account":2}"#).await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"sessão já autenticada como conta 1"}"#
        );
        client
            .send(r#"{"type":"deposit","asset":"BRL","amount":7}"#)
            .await;
        client.recv().await;
        client.send(r#"{"type":"balance"}"#).await;
        assert!(
            client
                .recv()
                .await
                .contains(r#""asset":"BRL","available":7"#)
        );
    }

    #[tokio::test]
    async fn bad_lines_get_an_error_and_the_connection_keeps_working() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::login(address, 1).await;

        client.send("isto não é json").await;
        assert!(
            client
                .recv()
                .await
                .starts_with(r#"{"type":"error","message":"mensagem inválida:"#)
        );
        client
            .send(r#"{"type":"deposit","asset":"OURO","amount":1}"#)
            .await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"ativo desconhecido \"OURO\""}"#
        );
        client.send_raw(b"\xff\xfe\n").await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"linha não é UTF-8"}"#
        );
        client.send("").await;
        client.send(r#"{"type":"balance"}"#).await;
        assert!(client.recv().await.starts_with(r#"{"type":"balances""#));
    }

    // O TCP não tem mensagens: o servidor tem que remontar a linha venha ela em pedaços
    // ou grudada na seguinte.
    #[tokio::test]
    async fn messages_are_framed_by_newline_not_by_writes() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::connect(address).await;

        for piece in [&b"{\"type\":\"lo"[..], b"gin\",\"acc", b"ount\":1}"] {
            client.send_raw(piece).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        client.send_raw(b"\n").await;
        assert_eq!(client.recv().await, r#"{"type":"logged_in","account":1}"#);

        client
            .send_raw(
                b"{\"type\":\"deposit\",\"asset\":\"BRL\",\"amount\":5}\r\n{\"type\":\"deposit\",\"asset\":\"BRL\",\"amount\":6}\n",
            )
            .await;
        assert_eq!(
            client.recv().await,
            r#"{"type":"deposited","asset":"BRL","amount":5}"#
        );
        assert_eq!(
            client.recv().await,
            r#"{"type":"deposited","asset":"BRL","amount":6}"#
        );
    }

    #[tokio::test]
    async fn oversized_line_closes_only_that_connection() {
        let address = start(Sequencer::in_memory()).await;
        let mut flooder = Client::connect(address).await;
        let mut bystander = Client::login(address, 2).await;

        // Exatamente um byte além do teto: o servidor consome tudo antes de fechar, então o
        // fechamento é limpo (sobra não lida faria o kernel responder com RST).
        flooder.send_raw(&vec![b'x'; MAX_LINE + 1]).await;
        assert_eq!(
            flooder.recv().await,
            format!(r#"{{"type":"error","message":"linha passa de {MAX_LINE} bytes"}}"#)
        );
        assert!(flooder.closed().await);

        bystander.send(r#"{"type":"balance"}"#).await;
        assert!(bystander.recv().await.starts_with(r#"{"type":"balances""#));
    }

    #[tokio::test]
    async fn line_of_exactly_the_limit_is_accepted() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::login(address, 1).await;
        let message = r#"{"type":"balance"}"#;
        let padded = format!("{message}{}", " ".repeat(MAX_LINE - message.len()));

        client.send(&padded).await;

        assert!(client.recv().await.starts_with(r#"{"type":"balances""#));
    }

    #[tokio::test]
    async fn client_that_drops_mid_session_does_not_affect_others() {
        let address = start(Sequencer::in_memory()).await;
        let mut leaver = Client::login(address, 1).await;
        leaver.fund().await;
        leaver
            .send(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":1}"#,
            )
            .await;
        drop(leaver);

        let mut stayer = Client::login(address, 2).await;
        stayer.fund().await;
        stayer
            .send(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":1}"#,
            )
            .await;
        assert!(stayer.recv().await.starts_with(r#"{"type":"fill""#));
    }

    #[tokio::test]
    async fn stopped_core_ends_the_session_with_an_error() {
        let address = start(Sequencer::exhausted()).await;
        let mut client = Client::login(address, 1).await;

        client
            .send(r#"{"type":"deposit","asset":"BRL","amount":1}"#)
            .await;

        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"o núcleo parou"}"#
        );
        assert!(client.closed().await);
    }

    #[test]
    fn accept_failures_are_classified() {
        use io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted, InvalidInput};

        for kind in [ConnectionAborted, ConnectionReset, Interrupted] {
            assert_eq!(
                classify(&io::Error::from(kind)),
                AcceptFailure::Skip,
                "{kind:?}"
            );
        }
        for code in [EMFILE, ENFILE, ENOBUFS, ENOMEM] {
            assert_eq!(
                classify(&io::Error::from_raw_os_error(code)),
                AcceptFailure::Pause(RESOURCE_PAUSE),
                "errno {code}"
            );
        }
        assert_eq!(
            classify(&io::Error::from(InvalidInput)),
            AcceptFailure::Fatal
        );
        let bad_descriptor = 9;
        assert_eq!(
            classify(&io::Error::from_raw_os_error(bad_descriptor)),
            AcceptFailure::Fatal
        );
    }

    #[tokio::test]
    async fn connections_beyond_the_limit_are_told_and_closed() {
        let limits = Limits {
            max_connections: NonZeroUsize::new(2).unwrap(),
            ..ROOMY
        };
        let address = start_with(Sequencer::in_memory(), limits).await;
        let first = Client::login(address, 1).await;
        let _second = Client::login(address, 2).await;

        let mut third = Client::connect(address).await;
        assert_eq!(
            third.recv().await,
            r#"{"type":"error","message":"servidor cheio"}"#
        );
        assert!(third.closed().await);

        drop(first);
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let mut retry = Client::connect(address).await;
            retry.send(r#"{"type":"login","account":3}"#).await;
            if retry.recv().await == r#"{"type":"logged_in","account":3}"# {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "a vaga não voltou");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    const BRIEF: Limits = Limits {
        idle_timeout: Duration::from_millis(150),
        ..ROOMY
    };

    #[tokio::test]
    async fn idle_session_is_told_and_closed() {
        let address = start_with(Sequencer::in_memory(), BRIEF).await;
        let mut client = Client::login(address, 1).await;

        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"sessão ociosa"}"#
        );
        assert!(client.closed().await);
    }

    #[tokio::test]
    async fn line_trickled_slower_than_the_timeout_is_cut() {
        let address = start_with(Sequencer::in_memory(), BRIEF).await;
        let mut client = Client::login(address, 1).await;

        for piece in [&b"{\"type\""[..], b":\"bal", b"ance\"}"] {
            client.send_raw(piece).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        }

        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"sessão ociosa"}"#
        );
    }

    #[tokio::test]
    async fn active_session_outlives_the_timeout() {
        let address = start_with(Sequencer::in_memory(), BRIEF).await;
        let mut client = Client::login(address, 1).await;

        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            client.send(r#"{"type":"balance"}"#).await;
            assert!(client.recv().await.starts_with(r#"{"type":"balances""#));
        }
    }

    // Critério da F4, "borda não bloqueia núcleo", no caso mais direto: um cliente que só
    // escreve e nunca lê enche os buffers do socket e prende a própria sessão na escrita.
    // Ninguém mais pode sentir isso.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_that_never_reads_holds_back_only_itself() {
        let address = start(Sequencer::in_memory()).await;
        let mut hog = Client::login(address, 9).await;
        let written = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&written);
        let flood = tokio::spawn(async move {
            let burst = b"{\"type\":\"balance\"}\n".repeat(256);
            while hog.writer.write_all(&burst).await.is_ok() {
                counter.fetch_add(burst.len(), Ordering::Relaxed);
            }
        });

        let deadline = tokio::time::Instant::now() + PATIENCE * 2;
        let mut last = usize::MAX;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = written.load(Ordering::Relaxed);
            if now == last {
                break;
            }
            last = now;
            assert!(
                tokio::time::Instant::now() < deadline,
                "o cliente nunca travou"
            );
        }

        let mut alice = Client::login(address, 1).await;
        let mut bob = Client::login(address, 2).await;
        alice.fund().await;
        bob.fund().await;
        alice
            .send(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":1}"#,
            )
            .await;
        alice.recv().await;
        bob.send(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":1}"#,
        )
        .await;
        assert!(bob.recv().await.starts_with(r#"{"type":"fill""#));

        assert!(
            !flood.is_finished(),
            "o cliente que não lê continua travado"
        );
        assert_eq!(written.load(Ordering::Relaxed), last);
        flood.abort();
    }
}
