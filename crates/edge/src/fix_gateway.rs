//! Porta FIX da borda (F6, seção 4.5). O codec, a sessão e a tradução de aplicação são
//! puros; daqui só sai o que toca o mundo: o socket, o relógio de parede (decisão 8: é a
//! borda que o lê) e a fila para o núcleo.
//!
//! O estado FIX de uma conta (seqs, mensagens enviadas, ordens por `ClOrdID`) vive no
//! [`Registry`] e é emprestado à conexão enquanto ela durar. É isso que faz uma reconexão
//! retomar de onde a anterior caiu, e que deixa uma conta com no máximo uma conexão.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use exchange_core::domain::AccountId;
use exchange_fix::{
    Action, Envelope, Frame, Message, Role, Session, SessionConfig, Text, UtcTimestamp, decode,
    encode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::core_thread::CoreHandle;
use crate::fix_app::{FixOrders, Request};
use crate::market::HOUSE;
use crate::server::{AcceptFailure, Limits, classify};

/// O `TargetCompID` que os clientes usam para falar com a bolsa.
pub const EXCHANGE_COMP_ID: &str = "EXCHANGE";

/// Prazo para o Logon chegar. Antes dele ainda não há sessão nem heartbeat combinado.
const LOGON_TIMEOUT: Duration = Duration::from_secs(10);

/// De quanto em quanto tempo a sessão confere heartbeat e `TestRequest`; menor que qualquer
/// intervalo que faça sentido negociar, que é em segundos.
const TICK: Duration = Duration::from_secs(1);

const READ_CHUNK: usize = 4096;

/// Só um placeholder: o acceptor adota o intervalo que vier no Logon do cliente.
const PROPOSED_HEARTBEAT: NonZeroU32 = NonZeroU32::new(30).expect("30 não é zero");

struct FixAccount {
    session: Session,
    orders: FixOrders,
}

enum Slot {
    Idle(Box<FixAccount>),
    Connected,
}

/// O estado FIX de todas as contas que já se conectaram. O lock só é tomado para pegar e
/// devolver o estado, nunca durante I/O.
#[derive(Default)]
pub struct Registry {
    accounts: Mutex<HashMap<AccountId, Slot>>,
}

impl Registry {
    /// `None` se a conta já tem uma conexão ativa.
    fn checkout(&self, account: AccountId, comp_id: Text) -> Option<FixAccount> {
        let mut accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        match accounts.insert(account, Slot::Connected) {
            Some(Slot::Connected) => None,
            Some(Slot::Idle(state)) => Some(*state),
            None => Some(FixAccount {
                session: Session::new(SessionConfig {
                    role: Role::Acceptor,
                    sender: Text::new(EXCHANGE_COMP_ID).expect("CompID fixo é válido"),
                    target: comp_id,
                    heartbeat_secs: PROPOSED_HEARTBEAT,
                }),
                orders: FixOrders::new(account),
            }),
        }
    }

    fn checkin(&self, account: AccountId, state: FixAccount) {
        let mut accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        accounts.insert(account, Slot::Idle(Box::new(state)));
    }
}

/// Aceita conexões FIX para sempre, com o mesmo limite de conexões e a mesma política de
/// erros de `accept` da borda JSON.
///
/// # Errors
/// Se o `accept` falhar de um jeito que [`classify`] considera fatal.
pub async fn serve(
    listener: TcpListener,
    core: CoreHandle,
    registry: Arc<Registry>,
    limits: Limits,
) -> io::Result<()> {
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
        // Sem vaga, fecha sem resposta: antes do Logon não há sessão para mandar Logout.
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            continue;
        };
        let core = core.clone();
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            connection(stream, &core, &registry).await;
            drop(slot);
        });
    }
}

async fn connection(stream: TcpStream, core: &CoreHandle, registry: &Registry) {
    let (mut reader, writer) = stream.into_split();
    let mut inbox = Vec::new();
    let first = tokio::time::timeout(LOGON_TIMEOUT, next_message(&mut reader, &mut inbox)).await;
    let Ok(Some(logon)) = first else {
        return;
    };
    let Some((account, comp_id)) = account_of(&logon) else {
        return;
    };
    let Some(mut state) = registry.checkout(account, comp_id) else {
        return;
    };
    // Erro de I/O ou núcleo parado só encerram esta conexão; o estado volta ao registro
    // do mesmo jeito, e a próxima conexão recupera o que faltou pelo resend.
    let _ = drive(&mut state, logon, reader, writer, inbox, core).await;
    state.session.disconnected();
    registry.checkin(account, state);
}

/// Quem está se logando: o `SenderCompID` é o número da conta, na forma canônica (sem zero
/// à esquerda, para que "7" e "007" não virem duas sessões da mesma conta), e o
/// `TargetCompID` tem que ser a bolsa. A conta da casa não negocia.
fn account_of(logon: &Message) -> Option<(AccountId, Text)> {
    let header = Envelope::parse(logon.clone()).ok()?.header;
    if header.target.as_bytes() != EXCHANGE_COMP_ID.as_bytes() {
        return None;
    }
    let digits = std::str::from_utf8(header.sender.as_bytes()).ok()?;
    let number: u64 = digits.parse().ok()?;
    let account = AccountId::new(number);
    (number.to_string() == digits && account != HOUSE).then_some((account, header.sender))
}

async fn drive(
    state: &mut FixAccount,
    logon: Message,
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    mut inbox: Vec<u8>,
    core: &CoreHandle,
) -> io::Result<()> {
    let mut actions = state.session.connect(now());
    actions.extend(state.session.receive(now(), logon));
    let mut ticker = tokio::time::interval(TICK);
    let mut chunk = [0; READ_CHUNK];
    loop {
        if perform(state, actions, &mut writer, core).await? == Flow::Stop {
            return Ok(());
        }
        actions = tokio::select! {
            read = reader.read(&mut chunk) => {
                let read = read?;
                if read == 0 {
                    return Ok(());
                }
                inbox.extend_from_slice(&chunk[..read]);
                let mut actions = Vec::new();
                while let Some(message) = take_frame(&mut inbox) {
                    actions.extend(state.session.receive(now(), message));
                }
                actions
            }
            _ = ticker.tick() => state.session.tick(now()),
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flow {
    Continue,
    Stop,
}

/// Executa as ações na ordem. Uma mensagem de aplicação vai ao núcleo e as respostas
/// voltam pela sessão, que lhes dá seq; elas entram no fim da fila, depois do que já
/// estava esperando, e por isso saem no fio na mesma ordem dos seqs.
async fn perform(
    state: &mut FixAccount,
    actions: Vec<Action>,
    writer: &mut OwnedWriteHalf,
    core: &CoreHandle,
) -> io::Result<Flow> {
    let mut queue = VecDeque::from(actions);
    while let Some(action) = queue.pop_front() {
        match action {
            Action::Send(message) => {
                let mut bytes = Vec::new();
                encode(&message, &mut bytes).map_err(io::Error::other)?;
                writer.write_all(&bytes).await?;
            }
            Action::Deliver { seq, app } => {
                let replies = match state.orders.request(seq, &app) {
                    Request::Reply(reply) => vec![reply],
                    Request::Submit { intent, pending } => {
                        let events = core.submit(intent).await.map_err(io::Error::other)?;
                        state.orders.outcome(pending, &events)
                    }
                };
                for reply in replies {
                    queue.extend(state.session.send(now(), reply));
                }
            }
            Action::Disconnect => return Ok(Flow::Stop),
        }
    }
    Ok(Flow::Continue)
}

async fn next_message(reader: &mut OwnedReadHalf, inbox: &mut Vec<u8>) -> Option<Message> {
    let mut chunk = [0; READ_CHUNK];
    loop {
        if let Some(message) = take_frame(inbox) {
            return Some(message);
        }
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => inbox.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Tira do buffer o próximo quadro completo. Quadro corrompido é pulado sem chegar à
/// sessão (D5): o seq que ele levava vira buraco, e o buraco se resolve com resend.
fn take_frame(inbox: &mut Vec<u8>) -> Option<Message> {
    loop {
        match decode(inbox) {
            Frame::Complete { message, consumed } => {
                inbox.drain(..consumed);
                return Some(message);
            }
            Frame::Garbled { skip, .. } => {
                inbox.drain(..skip);
            }
            Frame::Incomplete => return None,
        }
    }
}

/// O relógio de parede, lido aqui na borda e passado como dado (decisão 8). Um relógio
/// antes de 1970 ou depois de 9999 vira o limite mais próximo em vez de derrubar a sessão.
fn now() -> UtcTimestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    UtcTimestamp::from_unix_millis(millis).unwrap_or(UtcTimestamp::MAX)
}

#[cfg(test)]
mod tests;
