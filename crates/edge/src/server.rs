//! Servidor TCP da borda (seção 4.4). O TCP entrega um fluxo de bytes sem fronteiras; a
//! fronteira entre mensagens é convenção nossa: uma por linha, com teto de tamanho.

use std::collections::HashMap;
use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use exchange_core::domain::{AccountId, InstrumentId};
use exchange_protocol::{AssetBalance, ClientMessage, ServerMessage};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use crate::core_thread::{CoreError, CoreHandle, Reports};
use crate::market::{ASSETS, HOUSE};
use crate::market_data::{Feed, Update};
use crate::protocol::{self, Inbound};

/// Linha maior que isso sem `\n` não é protocolo: é erro e a conexão fecha, para um
/// cliente não fazer o servidor acumular memória sem fim (seção 6).
pub const MAX_LINE: usize = 4096;

/// Defesas contra exaustão de recursos (seção 6), além do teto de linha.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Sessões simultâneas; acima disso a conexão recebe "servidor cheio" e fecha.
    pub max_connections: NonZeroUsize,
    /// Tempo máximo para uma linha inteira chegar. Vale por linha, não por byte: um cliente
    /// que pinga um byte de vez em quando também é encerrado. Sessão que assina o feed fica
    /// isenta: ela só escuta, e o sinal de vida certo é o heartbeat da sessão FIX (F6).
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
/// sempre em nome da conta autenticada, cujos relatórios privados a sessão assina.
pub enum Session {
    AwaitingLogin,
    Active(Reports),
}

/// Trata uma mensagem conforme o estado da sessão, possivelmente mudando o estado, e
/// devolve as respostas na ordem em que devem ser escritas.
///
/// # Errors
/// [`CoreError`] se o núcleo parou ou cortou os relatórios da sessão: ela deve ser
/// encerrada.
pub async fn respond(
    state: &mut Session,
    subscriptions: &mut Subscriptions,
    message: ClientMessage,
    core: &CoreHandle,
) -> Result<Vec<ServerMessage>, CoreError> {
    let Session::Active(reports) = state else {
        let message = match message {
            ClientMessage::Subscribe { instrument, feed } => {
                return match protocol::subscription(instrument, feed) {
                    Ok((instrument, feed)) => subscriptions.subscribe(core, instrument, feed).await,
                    Err(problem) => Ok(vec![error(problem.to_string())]),
                };
            }
            other => other,
        };
        let ClientMessage::Login { account } = message else {
            return Ok(vec![error("faça login primeiro")]);
        };
        let account = AccountId::new(account);
        if account == HOUSE {
            return Ok(vec![error("a conta da casa não negocia")]);
        }
        *state = Session::Active(core.reports(account).await?);
        return Ok(vec![ServerMessage::LoggedIn {
            account: account.get(),
        }]);
    };
    let account = reports.account();
    match protocol::resolve(message, account) {
        Err(problem) => Ok(vec![error(problem.to_string())]),
        Ok(Inbound::Login(_)) => Ok(vec![error(format!(
            "sessão já autenticada como conta {}",
            account.get()
        ))]),
        Ok(Inbound::Balance) => Ok(vec![balances(core, account).await?]),
        Ok(Inbound::Submit(intent)) => {
            let submitted = reports.submit(intent).await?;
            Ok(submitted
                .earlier
                .iter()
                .chain([&submitted.events])
                .flatten()
                .flat_map(|event| protocol::from_event(account, event))
                .collect())
        }
        Ok(Inbound::Subscribe { instrument, feed }) => {
            subscriptions.subscribe(core, instrument, feed).await
        }
    }
}

/// O que a sessão assinou e o último `seq` já entregue em cada fluxo. Um único receptor do
/// `broadcast` serve todos os fluxos; a filtragem é daqui.
#[derive(Default)]
pub struct Subscriptions {
    receiver: Option<broadcast::Receiver<Update>>,
    delivered: HashMap<(InstrumentId, Feed), u64>,
}

impl Subscriptions {
    pub fn is_active(&self) -> bool {
        self.receiver.is_some()
    }

    /// Assina **antes** de pedir o snapshot (seção 4.3): nada que aconteça entre os dois fica
    /// de fora, e o que o snapshot já cobre é filtrado por `forward`.
    async fn subscribe(
        &mut self,
        core: &CoreHandle,
        instrument: InstrumentId,
        feed: Feed,
    ) -> Result<Vec<ServerMessage>, CoreError> {
        if self.receiver.is_none() {
            self.receiver = Some(core.subscribe());
        }
        let Some(snapshot) = core.snapshot(instrument, feed).await? else {
            return Ok(vec![error("instrumento desconhecido")]);
        };
        self.delivered.insert((instrument, feed), snapshot.seq);
        Ok(vec![ServerMessage::from(&snapshot)])
    }

    /// Decide se o incremento vai para este cliente: só de fluxo assinado e só com `seq`
    /// além do último entregue, que passa a ser este. Um salto de sequência (o assinante
    /// ficou para trás e perdeu mensagens) passa adiante: o buraco tem que ficar visível
    /// para o cliente pedir um snapshot novo.
    pub fn forward(&mut self, update: &Update) -> bool {
        let Some(delivered) = self.delivered.get_mut(&(update.instrument, update.feed)) else {
            return false;
        };
        if update.seq <= *delivered {
            return false;
        }
        *delivered = update.seq;
        true
    }

    async fn next(&mut self) -> Result<Update, RecvError> {
        match &mut self.receiver {
            Some(receiver) => receiver.recv().await,
            None => std::future::pending().await,
        }
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
            asset: name.to_string(),
            available: balance.available.get(),
            reserved: balance.reserved.get(),
        });
    }
    Ok(ServerMessage::Balances { balances })
}

async fn session(stream: TcpStream, core: CoreHandle, idle_timeout: Duration) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut state = Session::AwaitingLogin;
    let mut subscriptions = Subscriptions::default();
    // A linha só é limpa depois de completa: se o select! cancelar a leitura no meio para
    // entregar um incremento, os bytes já lidos ficam aqui e a próxima leitura continua.
    let mut line = Vec::new();
    let mut deadline = Instant::now() + idle_timeout;
    loop {
        let (replies, keep_open) = tokio::select! {
            read = read_line(&mut reader, &mut line) => match read {
                Err(_) | Ok(Read::Closed) => return,
                Ok(Read::TooLong) => (
                    vec![error(format!("linha passa de {MAX_LINE} bytes"))],
                    false,
                ),
                Ok(Read::Line) => {
                    let outcome = handle(&line, &mut state, &mut subscriptions, &core).await;
                    line.clear();
                    deadline = Instant::now() + idle_timeout;
                    outcome
                }
            },
            update = subscriptions.next(), if subscriptions.is_active() => match update {
                Ok(update) if subscriptions.forward(&update) => {
                    (vec![ServerMessage::from(&update)], true)
                }
                // Ficou para trás: as mensagens perdidas aparecem como salto de sequência
                // no próximo incremento, e o cliente pede um snapshot novo.
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => (vec![error(CoreError::Stopped.to_string())], false),
            },
            reported = next_report(&mut state) => match reported {
                Ok(messages) => (messages, true),
                Err(problem) => (vec![error(problem.to_string())], false),
            },
            () = tokio::time::sleep_until(deadline), if !subscriptions.is_active() => {
                (vec![error("sessão ociosa")], false)
            }
        };
        if write(&mut writer, &replies).await.is_err() || !keep_open {
            return;
        }
    }
}

/// O que chegou para a conta vindo do comando de outra sessão; antes do login, nunca.
async fn next_report(state: &mut Session) -> Result<Vec<ServerMessage>, CoreError> {
    let Session::Active(reports) = state else {
        return std::future::pending().await;
    };
    let account = reports.account();
    let events = reports.next().await?;
    Ok(events
        .iter()
        .flat_map(|event| protocol::from_event(account, event))
        .collect())
}

async fn handle(
    line: &[u8],
    state: &mut Session,
    subscriptions: &mut Subscriptions,
    core: &CoreHandle,
) -> (Vec<ServerMessage>, bool) {
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
    match respond(state, subscriptions, message, core).await {
        Ok(replies) => (replies, true),
        Err(stopped) => (vec![error(stopped.to_string())], false),
    }
}

enum Read {
    Line,
    Closed,
    TooLong,
}

/// Lê até `\n` sem nunca guardar mais que `MAX_LINE + 1` bytes, contando o que já estava
/// em `line` de leituras canceladas. Pode ser cancelada a qualquer momento: o que foi lido
/// fica em `line`. A linha sai sem o `\n` (e sem um `\r` antes dele, para clientes que
/// mandam CRLF).
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> io::Result<Read> {
    let budget = (MAX_LINE + 1).saturating_sub(line.len());
    let limit = u64::try_from(budget).expect("teto pequeno cabe em u64");
    let read = reader.take(limit).read_until(b'\n', line).await?;
    if read == 0 && line.is_empty() {
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
    use crate::market::{INSTRUMENTS, new_market_data};
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
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"taker","order":2,"lots":2,"price":10000,"fee":20}"#
        );

        bob.send(r#"{"type":"balance"}"#).await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"balances","balances":[{"asset":"TESTE","available":100200,"reserved":0},{"asset":"BRL","available":99979980,"reserved":0},{"asset":"OUTRO","available":0,"reserved":0}]}"#
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

    fn feed_update(feed: Feed, seq: u64) -> Update {
        Update {
            instrument: INSTRUMENTS[0].1,
            feed,
            seq,
            body: crate::market_data::Body::Trade {
                price: exchange_core::domain::Price::new(10_000).unwrap(),
                lots: exchange_core::domain::Lots::new(1).unwrap(),
            },
        }
    }

    fn subscribed_at(feed: Feed, seq: u64) -> Subscriptions {
        let mut subscriptions = Subscriptions::default();
        subscriptions
            .delivered
            .insert((INSTRUMENTS[0].1, feed), seq);
        subscriptions
    }

    #[test]
    fn forward_skips_streams_that_were_not_subscribed() {
        let mut subscriptions = subscribed_at(Feed::L2, 5);

        assert!(!subscriptions.forward(&feed_update(Feed::L1, 6)));
        assert!(!subscriptions.forward(&Update {
            instrument: exchange_core::domain::InstrumentId::new(99),
            ..feed_update(Feed::L2, 6)
        }));
    }

    #[test]
    fn forward_skips_what_was_already_delivered() {
        let mut subscriptions = subscribed_at(Feed::L2, 5);

        assert!(!subscriptions.forward(&feed_update(Feed::L2, 4)));
        assert!(!subscriptions.forward(&feed_update(Feed::L2, 5)));
        assert!(subscriptions.forward(&feed_update(Feed::L2, 6)));
        assert!(!subscriptions.forward(&feed_update(Feed::L2, 6)));
        assert!(subscriptions.forward(&feed_update(Feed::L2, 7)));
    }

    #[test]
    fn forward_lets_a_sequence_jump_through() {
        let mut subscriptions = subscribed_at(Feed::L2, 5);

        assert!(subscriptions.forward(&feed_update(Feed::L2, 9)));
        assert!(!subscriptions.forward(&feed_update(Feed::L2, 8)));
    }

    const FEED_SELL: &str =
        r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":5}"#;
    const FEED_BUY: &str =
        r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":2}"#;

    // O segundo instrumento pela rede, com a especificação dele (tick 5, lot 10, colar 5%):
    // - a compra a mercado de Bob em OUTRO/BRL precisa travar 2.109 por lot (proteção
    //   2005 + ⌈2005 × 5%⌉ = 2106, mais a taxa de 1 lot, 3), mas os 10.000 de BRL dele
    //   têm 9.009 travados pela compra em TESTE/BRL (3 × (3000 + 3)): sobra 991, rejeita;
    // - cancelada a compra em TESTE/BRL, 2 lots a 2005 custam 4.010 mais 5 de taxa;
    // - quem assina OUTRO/BRL vê só os incrementos dele, com sequência própria, e nada do
    //   que Bob fez em TESTE/BRL.
    #[tokio::test]
    async fn second_instrument_trades_on_its_own_book_and_feed() {
        let address = start(Sequencer::in_memory()).await;
        let mut watcher = Client::connect(address).await;
        watcher
            .send(r#"{"type":"subscribe","instrument":"OUTRO/BRL","feed":"l2"}"#)
            .await;
        assert_eq!(
            watcher.recv().await,
            r#"{"type":"snapshot","instrument":"OUTRO/BRL","feed":"l2","seq":0,"bids":[],"asks":[]}"#
        );

        let mut alice = Client::login(address, 1).await;
        alice
            .send(r#"{"type":"deposit","asset":"OUTRO","amount":1000}"#)
            .await;
        assert_eq!(
            alice.recv().await,
            r#"{"type":"deposited","asset":"OUTRO","amount":1000}"#
        );
        alice
            .send(
                r#"{"type":"limit","instrument":"OUTRO/BRL","side":"sell","price":2005,"lots":3}"#,
            )
            .await;
        assert_eq!(
            alice.recv().await,
            r#"{"type":"rested","instrument":"OUTRO/BRL","order":1,"lots":3}"#
        );

        let mut bob = Client::login(address, 2).await;
        bob.send(r#"{"type":"deposit","asset":"BRL","amount":10000}"#)
            .await;
        bob.recv().await;
        bob.send(r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":3000,"lots":3}"#)
            .await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"rested","instrument":"TESTE/BRL","order":2,"lots":3}"#
        );
        bob.send(r#"{"type":"market","instrument":"OUTRO/BRL","side":"buy","lots":1}"#)
            .await;
        assert!(
            bob.recv()
                .await
                .starts_with(r#"{"type":"rejected","order":3,"code":"insufficient_funds""#)
        );
        bob.send(r#"{"type":"cancel","instrument":"TESTE/BRL","order":2}"#)
            .await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"cancelled","instrument":"TESTE/BRL","order":2,"lots":3}"#
        );
        bob.send(r#"{"type":"market","instrument":"OUTRO/BRL","side":"buy","lots":2}"#)
            .await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"fill","instrument":"OUTRO/BRL","role":"taker","order":4,"lots":2,"price":2005,"fee":5}"#
        );
        bob.send(r#"{"type":"balance"}"#).await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"balances","balances":[{"asset":"TESTE","available":0,"reserved":0},{"asset":"BRL","available":5985,"reserved":0},{"asset":"OUTRO","available":20,"reserved":0}]}"#
        );

        for expected in [
            r#"{"type":"depth","instrument":"OUTRO/BRL","feed":"l2","seq":1,"side":"sell","price":2005,"lots":3}"#,
            r#"{"type":"trade","instrument":"OUTRO/BRL","feed":"l2","seq":2,"price":2005,"lots":2}"#,
            r#"{"type":"depth","instrument":"OUTRO/BRL","feed":"l2","seq":3,"side":"sell","price":2005,"lots":1}"#,
        ] {
            assert_eq!(watcher.recv().await, expected);
        }
    }

    /// Carol oferece 1 lot a 10000 e 5 a 10500; Alice arma um stop de compra a mercado de
    /// 2 lots com gatilho em 10000.
    async fn armed_stop(address: SocketAddr) -> (Client, Client) {
        let mut carol = Client::login(address, 3).await;
        carol.fund().await;
        for (price, lots) in [(10_000, 1), (10_500, 5)] {
            carol
                .send(&format!(
                    r#"{{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":{price},"lots":{lots}}}"#
                ))
                .await;
            carol.recv().await;
        }
        let mut alice = Client::login(address, 1).await;
        alice.fund().await;
        alice
            .send(
                r#"{"type":"stop","instrument":"TESTE/BRL","side":"buy","trigger":10000,"lots":2}"#,
            )
            .await;
        assert_eq!(
            alice.recv().await,
            r#"{"type":"armed","instrument":"TESTE/BRL","order":3,"side":"buy","trigger":10000,"lots":2}"#
        );
        (carol, alice)
    }

    // O stop de Alice disparado pela ordem de Bob: Alice fica sabendo pelo fluxo privado,
    // Carol recebe os dois fills como maker, e Bob vê só o próprio negócio, sem saber do
    // stop nem de quem eram as ordens do outro lado.
    #[tokio::test]
    async fn stop_fired_by_another_session_reaches_only_those_it_concerns() {
        let address = start(Sequencer::in_memory()).await;
        let (mut carol, mut alice) = armed_stop(address).await;
        let mut bob = Client::login(address, 2).await;
        bob.fund().await;

        bob.send(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":1}"#)
            .await;
        assert_eq!(
            bob.recv().await,
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"taker","order":4,"lots":1,"price":10000,"fee":10}"#
        );
        bob.send(r#"{"type":"balance"}"#).await;
        assert!(bob.recv().await.starts_with(r#"{"type":"balances""#));

        for expected in [
            r#"{"type":"triggered","instrument":"TESTE/BRL","order":3}"#,
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"taker","order":3,"lots":2,"price":10500,"fee":21}"#,
        ] {
            assert_eq!(alice.recv().await, expected);
        }
        for expected in [
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"maker","order":1,"lots":1,"price":10000,"fee":0}"#,
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"maker","order":2,"lots":2,"price":10500,"fee":0}"#,
        ] {
            assert_eq!(carol.recv().await, expected);
        }
    }

    // A ordem de Alice é executada pelo comando de Bob e Alice pede o cancelamento logo em
    // seguida: o fill que explica a recusa chega antes dela.
    #[tokio::test]
    async fn fill_from_another_session_arrives_before_the_refusal_it_explains() {
        let address = start(Sequencer::in_memory()).await;
        let mut alice = Client::login(address, 1).await;
        let mut bob = Client::login(address, 2).await;
        alice.fund().await;
        bob.fund().await;
        alice
            .send(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":2}"#,
            )
            .await;
        alice.recv().await;
        bob.send(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":2}"#)
            .await;
        bob.recv().await;

        alice
            .send(r#"{"type":"cancel","instrument":"TESTE/BRL","order":1}"#)
            .await;

        assert_eq!(
            alice.recv().await,
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"maker","order":1,"lots":2,"price":10000,"fee":0}"#
        );
        assert!(
            alice
                .recv()
                .await
                .starts_with(r#"{"type":"rejected","order":1,"code":"unknown_order""#)
        );
    }

    #[tokio::test]
    async fn stop_fired_by_its_own_session_is_reported_in_full() {
        let address = start(Sequencer::in_memory()).await;
        let (_carol, mut alice) = armed_stop(address).await;

        alice
            .send(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":1}"#)
            .await;
        for expected in [
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"taker","order":4,"lots":1,"price":10000,"fee":10}"#,
            r#"{"type":"triggered","instrument":"TESTE/BRL","order":3}"#,
            r#"{"type":"fill","instrument":"TESTE/BRL","role":"taker","order":3,"lots":2,"price":10500,"fee":21}"#,
        ] {
            assert_eq!(alice.recv().await, expected);
        }
    }

    async fn subscriber(address: SocketAddr, feed: &str) -> Client {
        let mut client = Client::connect(address).await;
        client
            .send(&format!(
                r#"{{"type":"subscribe","instrument":"TESTE/BRL","feed":"{feed}"}}"#
            ))
            .await;
        client
    }

    // O critério da F5 visto da rede: snapshot, depois os incrementos causados por outro
    // cliente, sem login, na sequência certa.
    #[tokio::test]
    async fn subscriber_gets_a_snapshot_then_the_book_changes_live() {
        let address = start(Sequencer::in_memory()).await;
        let mut seller = Client::login(address, 1).await;
        seller.fund().await;
        seller.send(FEED_SELL).await;
        seller.recv().await;

        let mut watcher = subscriber(address, "l2").await;
        assert_eq!(
            watcher.recv().await,
            r#"{"type":"snapshot","instrument":"TESTE/BRL","feed":"l2","seq":1,"bids":[],"asks":[{"price":10000,"lots":5}]}"#
        );

        let mut buyer = Client::login(address, 2).await;
        buyer.fund().await;
        buyer.send(FEED_BUY).await;
        buyer.recv().await;

        assert_eq!(
            watcher.recv().await,
            r#"{"type":"trade","instrument":"TESTE/BRL","feed":"l2","seq":2,"price":10000,"lots":2}"#
        );
        assert_eq!(
            watcher.recv().await,
            r#"{"type":"depth","instrument":"TESTE/BRL","feed":"l2","seq":3,"side":"sell","price":10000,"lots":3}"#
        );
    }

    #[tokio::test]
    async fn l1_subscriber_sees_top_and_trades_but_no_depth() {
        let address = start(Sequencer::in_memory()).await;
        let mut watcher = subscriber(address, "l1").await;
        assert_eq!(
            watcher.recv().await,
            r#"{"type":"snapshot","instrument":"TESTE/BRL","feed":"l1","seq":0,"bids":[],"asks":[]}"#
        );
        let mut trader = Client::login(address, 1).await;
        trader.fund().await;

        trader.send(FEED_SELL).await;
        trader.recv().await;
        trader.send(FEED_BUY).await;
        trader.recv().await;

        assert_eq!(
            watcher.recv().await,
            r#"{"type":"top","instrument":"TESTE/BRL","feed":"l1","seq":1,"bid":null,"ask":{"price":10000,"lots":5}}"#
        );
        assert_eq!(
            watcher.recv().await,
            r#"{"type":"trade","instrument":"TESTE/BRL","feed":"l1","seq":2,"price":10000,"lots":2}"#
        );
        assert_eq!(
            watcher.recv().await,
            r#"{"type":"top","instrument":"TESTE/BRL","feed":"l1","seq":3,"bid":null,"ask":{"price":10000,"lots":3}}"#
        );
    }

    #[tokio::test]
    async fn subscribing_to_an_unknown_instrument_is_an_error() {
        let address = start(Sequencer::in_memory()).await;
        let mut client = Client::connect(address).await;

        client
            .send(r#"{"type":"subscribe","instrument":"BTC/BRL","feed":"l2"}"#)
            .await;

        assert_eq!(
            client.recv().await,
            r#"{"type":"error","message":"instrumento desconhecido \"BTC/BRL\""}"#
        );
    }

    #[tokio::test]
    async fn subscribing_again_brings_a_fresh_snapshot() {
        let address = start(Sequencer::in_memory()).await;
        let mut watcher = subscriber(address, "l2").await;
        watcher.recv().await;
        let mut trader = Client::login(address, 1).await;
        trader.fund().await;
        trader.send(FEED_SELL).await;
        trader.recv().await;
        watcher.recv().await;

        watcher
            .send(r#"{"type":"subscribe","instrument":"TESTE/BRL","feed":"l2"}"#)
            .await;

        assert_eq!(
            watcher.recv().await,
            r#"{"type":"snapshot","instrument":"TESTE/BRL","feed":"l2","seq":1,"bids":[],"asks":[{"price":10000,"lots":5}]}"#
        );
    }

    #[tokio::test]
    async fn subscriber_is_not_cut_for_being_quiet() {
        let address = start_with(Sequencer::in_memory(), BRIEF).await;
        let mut watcher = subscriber(address, "l2").await;
        watcher.recv().await;

        tokio::time::sleep(BRIEF.idle_timeout * 3).await;
        let mut trader = Client::login(address, 1).await;
        trader.fund().await;
        trader.send(FEED_SELL).await;

        assert!(watcher.recv().await.starts_with(r#"{"type":"depth""#));
    }

    // Cancelamento seguro: o select! interrompe a leitura de uma linha pela metade para
    // entregar incrementos, e a linha tem que continuar inteira de onde parou.
    #[tokio::test]
    async fn half_a_line_survives_feed_updates_arriving_in_between() {
        let address = start(Sequencer::in_memory()).await;
        let mut watcher = subscriber(address, "l2").await;
        watcher.recv().await;
        let mut trader = Client::login(address, 1).await;
        trader.fund().await;

        watcher.send_raw(br#"{"type":"subscr"#).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        trader.send(FEED_SELL).await;
        trader.recv().await;
        assert!(watcher.recv().await.starts_with(r#"{"type":"depth""#));
        watcher
            .send_raw(b"ibe\",\"instrument\":\"TESTE/BRL\",\"feed\":\"l2\"}\n")
            .await;

        assert_eq!(
            watcher.recv().await,
            r#"{"type":"snapshot","instrument":"TESTE/BRL","feed":"l2","seq":1,"bids":[],"asks":[{"price":10000,"lots":5}]}"#
        );
    }

    #[tokio::test]
    async fn line_limit_counts_bytes_from_interrupted_reads() {
        let address = start(Sequencer::in_memory()).await;
        let mut watcher = subscriber(address, "l2").await;
        watcher.recv().await;
        let mut trader = Client::login(address, 1).await;
        trader.fund().await;

        watcher.send_raw(&vec![b' '; MAX_LINE - 10]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        trader.send(FEED_SELL).await;
        trader.recv().await;
        assert!(watcher.recv().await.starts_with(r#"{"type":"depth""#));
        watcher.send_raw(&[b' '; 11]).await;

        assert_eq!(
            watcher.recv().await,
            format!(r#"{{"type":"error","message":"linha passa de {MAX_LINE} bytes"}}"#)
        );
        assert!(watcher.closed().await);
    }
}
