//! A thread do núcleo (seção 2): o [`Sequencer`] mora sozinho numa thread comum, síncrona,
//! e a borda assíncrona fala com ele só por canais. A ordem em que os pedidos saem do canal
//! é a ordem canônica do sistema — a do log.

use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::thread::{self, JoinHandle};

use exchange_core::domain::{AccountId, AssetId};
use exchange_core::engine::Event;
use exchange_core::ledger::Balance;
use tokio::sync::{mpsc, oneshot};

use crate::sequencer::{Intent, Sequencer, SequencerError};

/// Um pedido com o endereço de volta: o núcleo responde pelo `oneshot` e nunca espera
/// ninguém para isso.
enum Request {
    Submit {
        intent: Intent,
        reply: oneshot::Sender<Vec<Event>>,
    },
    /// Consulta não vai para o log, mas passa pela mesma fila para ver o estado numa
    /// posição bem definida: depois de tudo que entrou antes dela.
    Balance {
        account: AccountId,
        asset: AssetId,
        reply: oneshot::Sender<Balance>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoreError {
    /// A thread do núcleo terminou (fail-stop ou desligamento): nada mais é aceito.
    Stopped,
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => write!(f, "o núcleo parou"),
        }
    }
}

impl std::error::Error for CoreError {}

/// O lado de envio, um por sessão. Quando todas as cópias somem, o núcleo desliga.
#[derive(Clone)]
pub struct CoreHandle {
    requests: mpsc::Sender<Request>,
}

impl CoreHandle {
    /// Espera vaga na fila (backpressure) e depois a resposta do núcleo.
    ///
    /// # Errors
    /// [`CoreError::Stopped`] se o núcleo parou antes de receber ou de responder.
    pub async fn submit(&self, intent: Intent) -> Result<Vec<Event>, CoreError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Submit { intent, reply })
            .await
            .map_err(|_| CoreError::Stopped)?;
        response.await.map_err(|_| CoreError::Stopped)
    }

    /// # Errors
    /// [`CoreError::Stopped`] se o núcleo parou antes de receber ou de responder.
    pub async fn balance(&self, account: AccountId, asset: AssetId) -> Result<Balance, CoreError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Balance {
                account,
                asset,
                reply,
            })
            .await
            .map_err(|_| CoreError::Stopped)?;
        response.await.map_err(|_| CoreError::Stopped)
    }
}

/// Move o `sequencer` para uma thread dedicada. A fila aceita até `capacity` pedidos
/// pendentes; cheia, quem envia espera.
///
/// A thread termina quando todos os [`CoreHandle`] somem, devolvendo o `Sequencer`, ou ao
/// primeiro erro do `Sequencer` (fail-stop), devolvendo o erro.
///
/// # Errors
/// Se o sistema operacional não conseguir criar a thread.
pub fn spawn_core(
    sequencer: Sequencer,
    capacity: NonZeroUsize,
) -> io::Result<(CoreHandle, JoinHandle<Result<Sequencer, SequencerError>>)> {
    let (requests, receiver) = mpsc::channel(capacity.get());
    let thread = thread::Builder::new()
        .name("nucleo".to_string())
        .spawn(move || run_core(sequencer, receiver))?;
    Ok((CoreHandle { requests }, thread))
}

/// O laço do single writer: um pedido por vez, na ordem da fila.
fn run_core(
    mut sequencer: Sequencer,
    mut requests: mpsc::Receiver<Request>,
) -> Result<Sequencer, SequencerError> {
    while let Some(request) = requests.blocking_recv() {
        match request {
            Request::Submit { intent, reply } => {
                let events = sequencer.submit(intent)?;
                // Cliente que já foi embora: o comando aconteceu (está no log e aplicado),
                // só a resposta não tem para onde ir. O núcleo não espera ninguém.
                let _ = reply.send(events);
            }
            Request::Balance {
                account,
                asset,
                reply,
            } => {
                let _ = reply.send(sequencer.balance(account, asset));
            }
        }
    }
    Ok(sequencer)
}

#[cfg(test)]
mod tests {
    use exchange_core::domain::{Amount, Lots, OrderId, OrderKind, Price, Side, TimeInForce};

    use super::*;
    use crate::market::{BRL, INSTRUMENTS, TESTE};
    use crate::test_support::TempPath;

    const ALICE: AccountId = AccountId::new(1);

    fn capacity(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    fn fund(account: AccountId, asset: AssetId, amount: i64) -> Intent {
        Intent::Deposit {
            account,
            asset,
            amount: Amount::new(amount).unwrap(),
        }
    }

    fn limit(account: AccountId, side: Side, price: i64, lots: i64) -> Intent {
        Intent::Order {
            account,
            instrument: INSTRUMENTS[0].1,
            side,
            kind: OrderKind::Limit(Price::new(price).unwrap()),
            lots: Lots::new(lots).unwrap(),
            time_in_force: TimeInForce::Gtc,
        }
    }

    fn cancel(account: AccountId, order: OrderId) -> Intent {
        Intent::Cancel {
            account,
            instrument: INSTRUMENTS[0].1,
            order,
        }
    }

    fn shut_down(
        handle: CoreHandle,
        thread: JoinHandle<Result<Sequencer, SequencerError>>,
    ) -> Sequencer {
        drop(handle);
        thread
            .join()
            .expect("a thread do núcleo não deu panic")
            .expect("o núcleo desligou sem erro")
    }

    #[tokio::test]
    async fn handle_answers_exactly_what_the_sequencer_answers() {
        let intents = [
            fund(ALICE, BRL, 1_000_000),
            fund(ALICE, TESTE, 10_000),
            limit(ALICE, Side::Ask, 10_000, 5),
            limit(ALICE, Side::Bid, 10_000, 2),
            cancel(ALICE, OrderId::new(1)),
            cancel(ALICE, OrderId::new(1)),
        ];
        let mut direct = Sequencer::in_memory();
        let (handle, thread) = spawn_core(Sequencer::in_memory(), capacity(4)).unwrap();

        for intent in intents {
            let expected = direct.submit(intent).unwrap();
            assert_eq!(handle.submit(intent).await, Ok(expected));
        }
        assert_eq!(
            handle.balance(ALICE, BRL).await,
            Ok(direct.balance(ALICE, BRL))
        );

        let threaded = shut_down(handle, thread);
        assert_eq!(threaded.engine(), direct.engine());
    }

    /// Ordens de uma sessão: compras e vendas alternadas em volta de 10.000, para cruzar
    /// com as das outras sessões, e de vez em quando o cancelamento da última que descansou.
    async fn trade(handle: CoreHandle, session: u64) -> usize {
        let account = AccountId::new(session + 1);
        let mut answered = 0;
        let mut last_rested = None;
        for intent in [
            fund(account, BRL, 100_000_000),
            fund(account, TESTE, 1_000_000),
        ] {
            handle.submit(intent).await.unwrap();
            answered += 1;
        }
        for step in 0..40 {
            let side = if (session + step).is_multiple_of(2) {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = 9_900 + 10 * i64::try_from((step * 7 + session) % 21).unwrap();
            let lots = i64::try_from(step % 3 + 1).unwrap();
            let intent = match last_rested.take() {
                Some(order) if step % 10 == 9 => cancel(account, order),
                previous => {
                    last_rested = previous;
                    limit(account, side, price, lots)
                }
            };
            for event in handle.submit(intent).await.unwrap() {
                if let Event::Rested { order, .. } = event {
                    last_rested = Some(order);
                }
            }
            answered += 1;
        }
        answered
    }

    // Oito sessões disputam o canal ao mesmo tempo, em ordem imprevisível. A ordem que vale
    // é a do log: o estado da thread tem que ser exatamente o que o replay reconstrói.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_sessions_are_all_answered_and_the_log_replays_to_the_same_state() {
        let path = TempPath::new("concurrent");
        let (sequencer, _) = Sequencer::open(&path.0).unwrap();
        let (handle, thread) = spawn_core(sequencer, capacity(8)).unwrap();

        let sessions: Vec<_> = (0..8)
            .map(|session| tokio::spawn(trade(handle.clone(), session)))
            .collect();
        let mut answered = 0;
        for session in sessions {
            answered += session.await.unwrap();
        }
        let live = shut_down(handle, thread);

        let (replayed, report) = Sequencer::open(&path.0).unwrap();
        assert_eq!(answered, 8 * 42);
        assert_eq!(report.replayed, answered);
        assert_eq!(replayed.engine(), live.engine());
    }

    #[tokio::test]
    async fn abandoned_reply_does_not_stop_the_core() {
        let (handle, thread) = spawn_core(Sequencer::in_memory(), capacity(4)).unwrap();
        let (reply, response) = oneshot::channel();
        drop(response);

        handle
            .requests
            .send(Request::Submit {
                intent: fund(ALICE, BRL, 500),
                reply,
            })
            .await
            .unwrap();

        let balance = handle.balance(ALICE, BRL).await.unwrap();
        assert_eq!(balance.available, Amount::new(500).unwrap());
        shut_down(handle, thread);
    }

    #[tokio::test]
    async fn sequencer_error_stops_the_core_and_every_handle_sees_it() {
        let (handle, thread) = spawn_core(Sequencer::exhausted(), capacity(4)).unwrap();
        let other = handle.clone();

        assert_eq!(
            handle.submit(fund(ALICE, BRL, 1)).await,
            Err(CoreError::Stopped)
        );
        assert_eq!(
            other.submit(fund(ALICE, BRL, 1)).await,
            Err(CoreError::Stopped)
        );
        assert_eq!(other.balance(ALICE, BRL).await, Err(CoreError::Stopped));

        drop((handle, other));
        assert!(matches!(
            thread.join().unwrap(),
            Err(SequencerError::CounterExhausted)
        ));
    }

    #[tokio::test]
    async fn core_that_vanishes_before_answering_is_reported_as_stopped() {
        let (requests, mut receiver) = mpsc::channel(1);
        let core = thread::spawn(move || drop(receiver.blocking_recv()));
        let handle = CoreHandle { requests };

        assert_eq!(
            handle.submit(fund(ALICE, BRL, 1)).await,
            Err(CoreError::Stopped)
        );
        core.join().unwrap();
    }
}
