//! A thread do núcleo (seção 2): o [`Sequencer`] mora sozinho numa thread comum, síncrona,
//! e a borda assíncrona fala com ele só por canais. A ordem em que os pedidos saem do canal
//! é a ordem canônica do sistema — a do log.

use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::thread::{self, JoinHandle};

use exchange_core::domain::{AccountId, AssetId, InstrumentId};
use exchange_core::engine::Event;
use exchange_core::ledger::Balance;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::market_data::{Feed, MarketData, Snapshot, Update};
use crate::sequencer::{Intent, Sequencer, SequencerError};

/// Quantos pedidos o núcleo tira da fila por rodada. Limita o lote e, com ele, quanto o
/// último pedido espera pelo `fsync` dos que vieram antes.
const MAX_BATCH: usize = 256;

/// Um pedido com o endereço de volta: o núcleo responde pelo `oneshot` e nunca espera
/// ninguém para isso.
enum Request {
    Submit(Submission),
    /// Consulta não vai para o log, mas passa pela mesma fila para ver o estado numa
    /// posição bem definida: depois de tudo que entrou antes dela.
    Query(Query),
}

struct Submission {
    intent: Intent,
    reply: oneshot::Sender<Vec<Event>>,
}

enum Query {
    Balance {
        account: AccountId,
        asset: AssetId,
        reply: oneshot::Sender<Balance>,
    },
    /// Pela mesma fila dos comandos: o snapshot no `seq` N reflete exatamente tudo que
    /// entrou antes dele, e os incrementos seguintes começam em N + 1.
    Snapshot {
        instrument: InstrumentId,
        feed: Feed,
        reply: oneshot::Sender<Option<Snapshot>>,
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
    updates: broadcast::Sender<Update>,
}

impl CoreHandle {
    /// Espera vaga na fila (backpressure) e depois a resposta do núcleo.
    ///
    /// # Errors
    /// [`CoreError::Stopped`] se o núcleo parou antes de receber ou de responder.
    pub async fn submit(&self, intent: Intent) -> Result<Vec<Event>, CoreError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Submit(Submission { intent, reply }))
            .await
            .map_err(|_| CoreError::Stopped)?;
        response.await.map_err(|_| CoreError::Stopped)
    }

    /// # Errors
    /// [`CoreError::Stopped`] se o núcleo parou antes de receber ou de responder.
    pub async fn balance(&self, account: AccountId, asset: AssetId) -> Result<Balance, CoreError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Query(Query::Balance {
                account,
                asset,
                reply,
            }))
            .await
            .map_err(|_| CoreError::Stopped)?;
        response.await.map_err(|_| CoreError::Stopped)
    }

    /// Passa a receber todos os incrementos publicados daqui em diante. Assine **antes** de
    /// pedir o snapshot: o que chegar com `seq` já coberto por ele é descartado pelo
    /// [`crate::market_data::LocalBook`], e nada fica de fora.
    ///
    /// Quem não acompanha perde mensagens (`Lagged`) em vez de segurar o núcleo; o buraco
    /// aparece na sequência e se resolve com um snapshot novo.
    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }

    /// `None` se o instrumento não existe.
    ///
    /// # Errors
    /// [`CoreError::Stopped`] se o núcleo parou antes de receber ou de responder.
    pub async fn snapshot(
        &self,
        instrument: InstrumentId,
        feed: Feed,
    ) -> Result<Option<Snapshot>, CoreError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Query(Query::Snapshot {
                instrument,
                feed,
                reply,
            }))
            .await
            .map_err(|_| CoreError::Stopped)?;
        response.await.map_err(|_| CoreError::Stopped)
    }
}

/// Move o `sequencer` e o feed `market_data` para uma thread dedicada. A fila aceita até
/// `queue` pedidos pendentes; cheia, quem envia espera. O feed guarda até `feed`
/// incrementos para quem está atrasado; além disso, o atrasado perde mensagens.
///
/// A thread termina quando todos os [`CoreHandle`] somem, devolvendo o `Sequencer`, ou ao
/// primeiro erro do `Sequencer` (fail-stop), devolvendo o erro.
///
/// # Errors
/// Se o sistema operacional não conseguir criar a thread.
pub fn spawn_core(
    sequencer: Sequencer,
    market_data: MarketData,
    queue: NonZeroUsize,
    feed: NonZeroUsize,
) -> io::Result<(CoreHandle, JoinHandle<Result<Sequencer, SequencerError>>)> {
    let (requests, receiver) = mpsc::channel(queue.get());
    let (updates, _) = broadcast::channel(feed.get());
    let publisher = updates.clone();
    let thread = thread::Builder::new()
        .name("nucleo".to_string())
        .spawn(move || run_core(sequencer, market_data, receiver, &publisher))?;
    Ok((CoreHandle { requests, updates }, thread))
}

/// O laço do single writer. A cada rodada tira da fila o que já chegou (até
/// [`MAX_BATCH`]) e o processa na ordem da fila: submissões seguidas viram um lote com um
/// único `fsync` (group commit, decisão 5); cada consulta fecha o lote anterior, para ver o
/// estado depois de tudo que entrou antes dela e de nada que entrou depois.
///
/// O feed é alimentado no mesmo laço, com os mesmos eventos, logo depois de cada comando:
/// núcleo e feed nunca divergem.
fn run_core(
    mut sequencer: Sequencer,
    mut market_data: MarketData,
    mut requests: mpsc::Receiver<Request>,
    updates: &broadcast::Sender<Update>,
) -> Result<Sequencer, SequencerError> {
    while let Some(round) = next_round(&mut requests) {
        for step in steps(round) {
            match step {
                Step::Batch(batch) => {
                    let intents: Vec<Intent> = batch.iter().map(|pending| pending.intent).collect();
                    let results = sequencer.submit_batch(&intents)?;
                    for (pending, events) in batch.into_iter().zip(results) {
                        for update in market_data.apply(&events) {
                            // Sem assinante, o send falha; com assinante atrasado, ele perde
                            // mensagens. Em nenhum caso o núcleo espera.
                            let _ = updates.send(update);
                        }
                        // Cliente que já foi embora: o comando aconteceu (está no log e
                        // aplicado), só a resposta não tem para onde ir.
                        let _ = pending.reply.send(events);
                    }
                }
                Step::Query(query) => answer(&sequencer, &market_data, query),
            }
        }
    }
    Ok(sequencer)
}

/// Espera o primeiro pedido e junta, sem esperar, os que já estão na fila. `None` quando
/// todos os [`CoreHandle`] sumiram e a fila esvaziou.
fn next_round(requests: &mut mpsc::Receiver<Request>) -> Option<Vec<Request>> {
    let mut round = vec![requests.blocking_recv()?];
    while round.len() < MAX_BATCH {
        match requests.try_recv() {
            Ok(request) => round.push(request),
            Err(_) => break,
        }
    }
    Some(round)
}

enum Step {
    Batch(Vec<Submission>),
    Query(Query),
}

/// Agrupa as submissões seguidas em lotes, mantendo a ordem; cada consulta fica sozinha
/// entre o lote de antes e o de depois.
fn steps(round: Vec<Request>) -> Vec<Step> {
    let mut steps = Vec::new();
    for request in round {
        match (request, steps.last_mut()) {
            (Request::Submit(pending), Some(Step::Batch(batch))) => batch.push(pending),
            (Request::Submit(pending), _) => steps.push(Step::Batch(vec![pending])),
            (Request::Query(query), _) => steps.push(Step::Query(query)),
        }
    }
    steps
}

fn answer(sequencer: &Sequencer, market_data: &MarketData, query: Query) {
    match query {
        Query::Balance {
            account,
            asset,
            reply,
        } => {
            let _ = reply.send(sequencer.balance(account, asset));
        }
        Query::Snapshot {
            instrument,
            feed,
            reply,
        } => {
            let _ = reply.send(market_data.snapshot(instrument, feed));
        }
    }
}

#[cfg(test)]
mod tests {
    use exchange_core::domain::{Amount, Lots, OrderId, OrderKind, Price, Side, TimeInForce};

    use super::*;
    use crate::market::{BRL, INSTRUMENTS, TESTE, new_market_data};
    use crate::market_data::{Gap, LocalBook};
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
        let (handle, thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            capacity(4),
            capacity(1024),
        )
        .unwrap();

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
        let (handle, thread) =
            spawn_core(sequencer, new_market_data(), capacity(8), capacity(1024)).unwrap();

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
        let (handle, thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            capacity(4),
            capacity(1024),
        )
        .unwrap();
        let (reply, response) = oneshot::channel();
        drop(response);

        handle
            .requests
            .send(Request::Submit(Submission {
                intent: fund(ALICE, BRL, 500),
                reply,
            }))
            .await
            .unwrap();

        let balance = handle.balance(ALICE, BRL).await.unwrap();
        assert_eq!(balance.available, Amount::new(500).unwrap());
        shut_down(handle, thread);
    }

    #[tokio::test]
    async fn sequencer_error_stops_the_core_and_every_handle_sees_it() {
        let (handle, thread) = spawn_core(
            Sequencer::exhausted(),
            new_market_data(),
            capacity(4),
            capacity(1024),
        )
        .unwrap();
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
        let (updates, _) = broadcast::channel(1);
        let core = thread::spawn(move || drop(receiver.blocking_recv()));
        let handle = CoreHandle { requests, updates };

        assert_eq!(
            handle.submit(fund(ALICE, BRL, 1)).await,
            Err(CoreError::Stopped)
        );
        core.join().unwrap();
    }

    const TESTE_BRL: InstrumentId = INSTRUMENTS[0].1;

    fn drain(receiver: &mut broadcast::Receiver<Update>) -> Vec<Update> {
        let mut updates = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        updates
    }

    async fn snapshot(handle: &CoreHandle) -> Snapshot {
        handle
            .snapshot(TESTE_BRL, Feed::L2)
            .await
            .unwrap()
            .expect("o instrumento existe")
    }

    #[tokio::test]
    async fn subscriber_receives_exactly_what_the_producer_produces() {
        let intents = [
            fund(ALICE, BRL, 1_000_000),
            fund(ALICE, TESTE, 10_000),
            limit(ALICE, Side::Ask, 10_000, 5),
            limit(ALICE, Side::Bid, 10_000, 2),
            cancel(ALICE, OrderId::new(1)),
        ];
        let mut direct = Sequencer::in_memory();
        let mut producer = new_market_data();
        let (handle, thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            capacity(4),
            capacity(1024),
        )
        .unwrap();
        let mut receiver = handle.subscribe();

        let mut expected = Vec::new();
        for intent in intents {
            expected.extend(producer.apply(&direct.submit(intent).unwrap()));
            handle.submit(intent).await.unwrap();
        }

        assert_eq!(drain(&mut receiver), expected);
        shut_down(handle, thread);
    }

    // Seção 4.3 com concorrência real: assina, pede o snapshot enquanto outras sessões
    // negociam, aplica tudo que chegou e termina com o livro do produtor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn subscribe_then_snapshot_rebuilds_the_book_under_concurrent_trading() {
        let (handle, thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            capacity(8),
            capacity(65_536),
        )
        .unwrap();
        trade(handle.clone(), 100).await;

        let mut receiver = handle.subscribe();
        let sessions: Vec<_> = (0..4)
            .map(|session| tokio::spawn(trade(handle.clone(), session)))
            .collect();
        let mut local = LocalBook::from_snapshot(&snapshot(&handle).await);
        for session in sessions {
            session.await.unwrap();
        }

        for update in drain(&mut receiver) {
            local.apply(&update).unwrap();
        }
        assert_eq!(local.snapshot(), snapshot(&handle).await);
        shut_down(handle, thread);
    }

    // O mecanismo inteiro da seção 4.3: quem não lê perde mensagens sem segurar o núcleo,
    // vê o buraco na sequência e se recupera com um snapshot novo.
    #[tokio::test]
    async fn slow_subscriber_loses_updates_sees_the_gap_and_recovers() {
        let (handle, thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            capacity(4),
            capacity(4),
        )
        .unwrap();
        for intent in [fund(ALICE, BRL, 1_000_000), fund(ALICE, TESTE, 10_000)] {
            handle.submit(intent).await.unwrap();
        }
        let mut receiver = handle.subscribe();
        let mut local = LocalBook::from_snapshot(&snapshot(&handle).await);

        for step in 0..30 {
            let accepted = handle
                .submit(limit(ALICE, Side::Ask, 10_000 + 10 * step, 1))
                .await;
            assert!(
                accepted.is_ok(),
                "o núcleo atendeu mesmo com o assinante parado"
            );
        }

        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let survivor = receiver.try_recv().unwrap();
        let before = local.clone();
        assert!(matches!(local.apply(&survivor), Err(Gap { .. })));
        assert_eq!(local, before);

        let mut local = LocalBook::from_snapshot(&snapshot(&handle).await);
        for update in drain(&mut receiver) {
            local.apply(&update).unwrap();
        }
        handle
            .submit(limit(ALICE, Side::Ask, 10_500, 1))
            .await
            .unwrap();
        for update in drain(&mut receiver) {
            local.apply(&update).unwrap();
        }
        assert_eq!(local.snapshot(), snapshot(&handle).await);
        shut_down(handle, thread);
    }

    #[tokio::test]
    async fn snapshot_of_an_unknown_instrument_is_none() {
        let (handle, thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            capacity(4),
            capacity(4),
        )
        .unwrap();

        assert_eq!(
            handle.snapshot(InstrumentId::new(99), Feed::L1).await,
            Ok(None)
        );
        shut_down(handle, thread);
    }

    fn submission(intent: Intent) -> (Request, oneshot::Receiver<Vec<Event>>) {
        let (reply, response) = oneshot::channel();
        (Request::Submit(Submission { intent, reply }), response)
    }

    fn balance_query(account: AccountId, asset: AssetId) -> (Request, oneshot::Receiver<Balance>) {
        let (reply, response) = oneshot::channel();
        let query = Query::Balance {
            account,
            asset,
            reply,
        };
        (Request::Query(query), response)
    }

    /// O tamanho de cada lote, com `0` no lugar de cada consulta.
    fn shape(steps: &[Step]) -> Vec<usize> {
        steps
            .iter()
            .map(|step| match step {
                Step::Batch(batch) => batch.len(),
                Step::Query(_) => 0,
            })
            .collect()
    }

    #[test]
    fn queries_split_the_round_into_batches_in_order() {
        let submit = || submission(fund(ALICE, BRL, 1)).0;
        let query = || balance_query(ALICE, BRL).0;

        assert_eq!(shape(&steps(vec![])), Vec::<usize>::new());
        assert_eq!(shape(&steps(vec![submit(), submit(), submit()])), [3]);
        assert_eq!(shape(&steps(vec![query(), query()])), [0, 0]);
        assert_eq!(
            shape(&steps(vec![submit(), submit(), query(), submit()])),
            [2, 0, 1]
        );
        assert_eq!(shape(&steps(vec![query(), submit(), query()])), [0, 1, 0]);
    }

    #[test]
    fn round_takes_at_most_max_batch_requests() {
        let extra = 10;
        let (sender, mut receiver) = mpsc::channel(MAX_BATCH + extra);
        for _ in 0..MAX_BATCH + extra {
            assert!(sender.try_send(submission(fund(ALICE, BRL, 1)).0).is_ok());
        }
        drop(sender);

        assert_eq!(
            next_round(&mut receiver).map(|round| round.len()),
            Some(MAX_BATCH)
        );
        assert_eq!(
            next_round(&mut receiver).map(|round| round.len()),
            Some(extra)
        );
        assert!(next_round(&mut receiver).is_none());
    }

    // Tudo já está na fila quando o núcleo começa, então sai numa rodada só: a consulta do
    // meio precisa ver o primeiro depósito e não o segundo, mesmo com os dois no mesmo
    // `next_round`.
    #[test]
    fn query_inside_a_round_sees_exactly_what_came_before_it() {
        let (sender, receiver) = mpsc::channel(8);
        let (first, first_reply) = submission(fund(ALICE, BRL, 100));
        let (between, between_reply) = balance_query(ALICE, BRL);
        let (second, second_reply) = submission(fund(ALICE, BRL, 50));
        let (after, after_reply) = balance_query(ALICE, BRL);
        for request in [first, between, second, after] {
            assert!(sender.try_send(request).is_ok());
        }
        drop(sender);
        let (updates, _) = broadcast::channel(16);

        run_core(
            Sequencer::in_memory(),
            new_market_data(),
            receiver,
            &updates,
        )
        .unwrap();

        let available = |mut reply: oneshot::Receiver<Balance>| reply.try_recv().unwrap().available;
        assert_eq!(available(between_reply), Amount::new(100).unwrap());
        assert_eq!(available(after_reply), Amount::new(150).unwrap());
        assert!(first_reply.blocking_recv().is_ok());
        assert!(second_reply.blocking_recv().is_ok());
    }
}
