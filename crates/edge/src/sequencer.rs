use std::fmt;
use std::path::Path;

use exchange_core::domain::{
    AccountId, Amount, AssetId, InstrumentId, Lots, OrderId, OrderKind, OrderRequest, Seq, Side,
    TimeInForce,
};
use exchange_core::engine::{Command, Engine, Event};
use exchange_core::ledger::Balance;
use exchange_core::record::{LogRecord, RecordError};
use exchange_wal::{Wal, WalError};

use crate::market;

/// O que um cliente pede, sem os números que só o núcleo atribui: o `seq` (posição no log)
/// e o id da ordem. A borda descreve a intenção; o [`Sequencer`] a transforma em
/// [`Command`], então nenhuma sessão consegue escolher a própria ordem no log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    Deposit {
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    },
    Order {
        account: AccountId,
        instrument: InstrumentId,
        side: Side,
        kind: OrderKind,
        lots: Lots,
        time_in_force: TimeInForce,
    },
    Cancel {
        account: AccountId,
        instrument: InstrumentId,
        order: OrderId,
    },
}

/// Próximos números a atribuir. O `seq` avança a cada comando gravado e é a posição no
/// log, que também dá a prioridade FIFO (decisão 8); o id de ordem só avança em ordens e
/// é o `#n` que o usuário vê.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Counters {
    next_seq: u64,
    next_order_id: u64,
}

impl Counters {
    const FRESH: Self = Self {
        next_seq: 1,
        next_order_id: 1,
    };
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReplayError {
    Record(RecordError),
    /// Todo comando aplicado é gravado com o próximo `seq`, então o log é contíguo a
    /// partir de 1: buraco ou repetição é bug, nunca crash.
    UnexpectedSeq {
        expected: u64,
        found: u64,
    },
    SeqExhausted,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::UnexpectedSeq { expected, found } => {
                write!(f, "seq {found} onde se esperava {expected}")
            }
            Self::SeqExhausted => write!(f, "seq chegou ao limite de u64"),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<RecordError> for ReplayError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

#[derive(Debug)]
pub enum SequencerError {
    Wal(WalError),
    /// Rabo rasgado o WAL já cortou; um quadro íntegro que não decodifica é bug ou
    /// formato incompatível, e seguir sem ele quebraria o replay. Fail-stop.
    Replay(ReplayError),
    CounterExhausted,
}

impl fmt::Display for SequencerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wal(error) => write!(f, "{error}"),
            Self::Replay(error) => write!(f, "log inválido: {error}"),
            Self::CounterExhausted => write!(f, "contador chegou ao limite de u64"),
        }
    }
}

impl std::error::Error for SequencerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wal(error) => Some(error),
            Self::Replay(error) => Some(error),
            Self::CounterExhausted => None,
        }
    }
}

impl From<WalError> for SequencerError {
    fn from(error: WalError) -> Self {
        Self::Wal(error)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootReport {
    pub replayed: usize,
    pub discarded: u64,
}

/// O núcleo rodando: único dono do engine, do WAL e dos contadores (single writer). Todo
/// comando passa por [`Sequencer::submit_batch`], que grava antes de aplicar (seção 4.6).
pub struct Sequencer {
    engine: Engine,
    wal: Option<Wal>,
    counters: Counters,
}

impl Sequencer {
    pub fn in_memory() -> Self {
        Self {
            engine: market::new_engine(),
            wal: None,
            counters: Counters::FRESH,
        }
    }

    /// Abre o WAL em `path` (cortando rabo rasgado) e reaplica o log antes de aceitar
    /// qualquer comando novo.
    ///
    /// # Errors
    /// [`SequencerError::Wal`] se o arquivo não puder ser aberto ou recuperado;
    /// [`SequencerError::Replay`] se um registro íntegro não puder ser reaplicado.
    pub fn open(path: &Path) -> Result<(Self, BootReport), SequencerError> {
        Self::open_observed(path, |_| {})
    }

    /// Como [`Sequencer::open`], entregando a `observer` os eventos de cada comando
    /// reaplicado, na ordem. É o que deixa quem deriva estado de eventos — o feed de market
    /// data — chegar ao mesmo lugar que o engine depois de um reinício.
    ///
    /// # Errors
    /// Os mesmos de [`Sequencer::open`].
    pub fn open_observed(
        path: &Path,
        observer: impl FnMut(&[Event]),
    ) -> Result<(Self, BootReport), SequencerError> {
        let recovery = Wal::open(path)?;
        let mut engine = market::new_engine();
        let counters =
            replay(&mut engine, &recovery.records, observer).map_err(SequencerError::Replay)?;
        let report = BootReport {
            replayed: recovery.records.len(),
            discarded: recovery.discarded,
        };
        let sequencer = Self {
            engine,
            wal: Some(recovery.wal),
            counters,
        };
        Ok((sequencer, report))
    }

    /// Um lote de um: ver [`Sequencer::submit_batch`].
    ///
    /// # Errors
    /// Os mesmos de [`Sequencer::submit_batch`].
    ///
    /// # Panics
    /// Nunca: um lote de uma intenção devolve exatamente um resultado.
    pub fn submit(&mut self, intent: Intent) -> Result<Vec<Event>, SequencerError> {
        let mut results = self.submit_batch(&[intent])?;
        Ok(results.pop().expect("um lote de um devolve um resultado"))
    }

    /// Transforma cada intenção em comando, com o próximo `seq` e, se for ordem, o próximo
    /// id; grava todos no WAL (se houver) com um único `fsync` (group commit, decisão 5) e
    /// só então aplica, em ordem. Devolve os eventos de cada intenção, na mesma ordem.
    ///
    /// O log e os eventos são exatamente os de chamar [`Sequencer::submit`] uma vez por
    /// intenção: o lote muda quantos `fsync`, nunca o que é gravado.
    ///
    /// # Errors
    /// [`SequencerError::Wal`] se a gravação falhar: nada do lote é aplicado e o WAL fica
    /// envenenado. [`SequencerError::CounterExhausted`] se um contador estourar, antes de
    /// gravar qualquer coisa.
    pub fn submit_batch(&mut self, intents: &[Intent]) -> Result<Vec<Vec<Event>>, SequencerError> {
        let mut counters = self.counters;
        let records = intents
            .iter()
            .map(|&intent| record_for(&mut counters, intent))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(wal) = self.wal.as_mut() {
            let payloads: Vec<Vec<u8>> = records.iter().map(LogRecord::to_bytes).collect();
            let slices: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
            wal.append_batch(&slices)?;
        }
        self.counters = counters;
        Ok(records
            .iter()
            .map(|record| self.engine.apply(record.command()))
            .collect())
    }

    pub fn balance(&self, account: AccountId, asset: AssetId) -> Balance {
        self.engine.balance(account, asset)
    }

    #[cfg(test)]
    pub(crate) fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Um sequencer cujo próximo `seq` já está no limite: o próximo `submit` falha. Só
    /// para exercitar o fail-stop de quem o usa.
    #[cfg(test)]
    pub(crate) fn exhausted() -> Self {
        let mut sequencer = Self::in_memory();
        sequencer.counters.next_seq = u64::MAX;
        sequencer
    }
}

/// O registro de `intent` com os próximos números de `counters`, que avançam. Quem chama
/// passa uma cópia e só a confirma depois de gravar: falha no meio não consome número.
fn record_for(counters: &mut Counters, intent: Intent) -> Result<LogRecord, SequencerError> {
    let seq = counters.next_seq;
    let next_seq = next(seq)?;
    let mut next_order_id = counters.next_order_id;
    let command = match intent {
        Intent::Deposit {
            account,
            asset,
            amount,
        } => Command::Deposit {
            account,
            asset,
            amount,
        },
        Intent::Order {
            account,
            instrument,
            side,
            kind,
            lots,
            time_in_force,
        } => {
            let request = OrderRequest {
                id: OrderId::new(next_order_id),
                account,
                instrument,
                side,
                kind,
                total: lots,
                seq: Seq::new(seq),
                time_in_force,
            };
            next_order_id = next(next_order_id)?;
            Command::Submit(request)
        }
        Intent::Cancel {
            account,
            instrument,
            order,
        } => Command::Cancel {
            instrument,
            account,
            order,
        },
    };
    *counters = Counters {
        next_seq,
        next_order_id,
    };
    Ok(LogRecord::new(Seq::new(seq), command))
}

fn next(counter: u64) -> Result<u64, SequencerError> {
    counter
        .checked_add(1)
        .ok_or(SequencerError::CounterExhausted)
}

/// Confere que `found` é exatamente o `seq` esperado e devolve o próximo esperado.
fn check_seq(expected: u64, found: Seq) -> Result<u64, ReplayError> {
    if found.get() != expected {
        return Err(ReplayError::UnexpectedSeq {
            expected,
            found: found.get(),
        });
    }
    expected.checked_add(1).ok_or(ReplayError::SeqExhausted)
}

/// Reaplica o log no engine. Os eventos vão só para o `observer`: para os clientes, eles já
/// foram respondidos na sessão que os gravou. Os contadores saem do próprio log, nada é
/// guardado à parte.
fn replay(
    engine: &mut Engine,
    records: &[Vec<u8>],
    mut observer: impl FnMut(&[Event]),
) -> Result<Counters, ReplayError> {
    let mut counters = Counters::FRESH;
    for bytes in records {
        let record = LogRecord::from_bytes(bytes)?;
        counters.next_seq = check_seq(counters.next_seq, record.seq())?;
        let command = record.command();
        if let Command::Submit(request) = command {
            let after = request.id.get().saturating_add(1);
            counters.next_order_id = counters.next_order_id.max(after);
        }
        observer(&engine.apply(command));
    }
    Ok(counters)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use exchange_core::domain::Price;
    use exchange_core::engine::RejectReason;
    use proptest::prelude::*;

    use super::*;
    use crate::market::{BRL, OUTRO, TESTE};
    use crate::market_data::{Feed, MarketData, Quote};
    use crate::test_support::TempPath;

    const ACCOUNT: AccountId = AccountId::new(1);
    const TESTE_BRL: InstrumentId = market::INSTRUMENTS[0].1;
    const OUTRO_BRL: InstrumentId = market::INSTRUMENTS[1].1;

    fn deposit(seq: u64) -> Vec<u8> {
        let command = Command::Deposit {
            account: ACCOUNT,
            asset: BRL,
            amount: Amount::new(100).unwrap(),
        };
        LogRecord::new(Seq::new(seq), command).to_bytes()
    }

    #[test]
    fn replay_accepts_a_contiguous_log() {
        let counters =
            replay(&mut market::new_engine(), &[deposit(1), deposit(2)], |_| {}).unwrap();

        assert_eq!(counters.next_seq, 3);
    }

    #[test]
    fn replay_rejects_a_gap_in_seq() {
        let error =
            replay(&mut market::new_engine(), &[deposit(1), deposit(3)], |_| {}).unwrap_err();

        assert_eq!(
            error,
            ReplayError::UnexpectedSeq {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn replay_rejects_a_repeated_seq() {
        let error =
            replay(&mut market::new_engine(), &[deposit(1), deposit(1)], |_| {}).unwrap_err();

        assert_eq!(
            error,
            ReplayError::UnexpectedSeq {
                expected: 2,
                found: 1
            }
        );
    }

    #[test]
    fn seq_at_the_limit_is_an_error_not_a_panic() {
        assert_eq!(
            check_seq(u64::MAX, Seq::new(u64::MAX)),
            Err(ReplayError::SeqExhausted)
        );
    }

    fn fund(asset: AssetId, amount: i64) -> Intent {
        Intent::Deposit {
            account: ACCOUNT,
            asset,
            amount: Amount::new(amount).unwrap(),
        }
    }

    fn order(side: Side, kind: OrderKind, lots: i64, time_in_force: TimeInForce) -> Intent {
        Intent::Order {
            account: ACCOUNT,
            instrument: TESTE_BRL,
            side,
            kind,
            lots: Lots::new(lots).unwrap(),
            time_in_force,
        }
    }

    fn limit(side: Side, price: i64, lots: i64, time_in_force: TimeInForce) -> Intent {
        order(
            side,
            OrderKind::Limit(Price::new(price).unwrap()),
            lots,
            time_in_force,
        )
    }

    fn limit_on(instrument: InstrumentId, side: Side, price: i64, lots: i64) -> Intent {
        Intent::Order {
            account: ACCOUNT,
            instrument,
            side,
            kind: OrderKind::Limit(Price::new(price).unwrap()),
            lots: Lots::new(lots).unwrap(),
            time_in_force: TimeInForce::Gtc,
        }
    }

    fn cancel(id: u64) -> Intent {
        Intent::Cancel {
            account: ACCOUNT,
            instrument: TESTE_BRL,
            order: OrderId::new(id),
        }
    }

    // Uma sessão que passa por todos os caminhos que mexem em estado: depósito,
    // descanso, fill parcial e total, IOC, cancelamento, rejeições e FOK expirada.
    fn scenario() -> Vec<Intent> {
        use Side::{Ask, Bid};
        use TimeInForce::{Fok, Gtc, Ioc};
        vec![
            fund(BRL, 100_000_000),
            fund(TESTE, 100_000),
            limit(Ask, 10_000, 5, Gtc),
            limit(Ask, 10_100, 3, Gtc),
            limit(Bid, 9_900, 4, Gtc),
            limit(Bid, 10_000, 2, Gtc),
            limit(Bid, 10_100, 4, Ioc),
            cancel(3),
            order(Ask, OrderKind::Market { protection: None }, 1, Gtc),
            limit(Bid, 10_005, 1, Gtc),
            limit(Bid, 100, 0, Gtc),
            limit(Ask, 10_200, 2, Fok),
            limit(Bid, 10_200, 2, Gtc),
            cancel(99),
        ]
    }

    /// O log da sessão, onde termina cada registro e o estado esperado depois de cada
    /// prefixo: `expected[n]` é o engine com só os `n` primeiros comandos aplicados,
    /// construído direto com `apply`, sem passar pelo replay que está sob teste.
    struct Recorded {
        bytes: Vec<u8>,
        ends: Vec<usize>,
        expected: Vec<(Engine, Counters)>,
    }

    fn record_scenario(name: &str) -> Recorded {
        let path = TempPath::new(&format!("{name}-scenario"));
        let (mut sequencer, _) = Sequencer::open(&path.0).unwrap();
        for intent in scenario() {
            sequencer.submit(intent).unwrap();
        }
        drop(sequencer);
        let bytes = fs::read(&path.0).unwrap();

        let decoded = exchange_wal::decode(&bytes);
        assert_eq!(
            decoded.valid_len,
            bytes.len(),
            "a sessão gravou um log íntegro"
        );
        let mut ends = Vec::new();
        let mut commands = Vec::new();
        for payload in decoded.records {
            ends.push(ends.last().unwrap_or(&0) + exchange_wal::HEADER_LEN + payload.len());
            commands.push(LogRecord::from_bytes(payload).unwrap().command());
        }
        let expected = (0..=commands.len())
            .map(|count| state_after(&commands[..count]))
            .collect();
        Recorded {
            bytes,
            ends,
            expected,
        }
    }

    fn state_after(commands: &[Command]) -> (Engine, Counters) {
        let mut engine = market::new_engine();
        let mut counters = Counters::FRESH;
        for &command in commands {
            if matches!(command, Command::Submit(_)) {
                counters.next_order_id += 1;
            }
            counters.next_seq += 1;
            engine.apply(command);
        }
        (engine, counters)
    }

    fn complete_before(ends: &[usize], cut: usize) -> usize {
        ends.iter().take_while(|&&end| end <= cut).count()
    }

    fn boot_from(path: &TempPath, bytes: &[u8]) -> (Engine, Counters) {
        fs::write(&path.0, bytes).unwrap();
        let (sequencer, _) = Sequencer::open(&path.0).unwrap();
        (sequencer.engine, sequencer.counters)
    }

    // Critério da F3: um crash pode deixar no disco qualquer prefixo do log. Para cada
    // um deles, o boot sobe e reconstrói exatamente o estado dos comandos inteiros.
    #[test]
    fn crash_at_any_byte_recovers_the_state_of_the_complete_commands() {
        let recorded = record_scenario("crash");
        let path = TempPath::new("crash");

        for cut in 0..=recorded.bytes.len() {
            let complete = complete_before(&recorded.ends, cut);
            let state = boot_from(&path, &recorded.bytes[..cut]);

            assert!(
                state == recorded.expected[complete],
                "corte em {cut} bytes deveria recuperar {complete} comandos"
            );
            let kept = if complete == 0 {
                0
            } else {
                recorded.ends[complete - 1]
            };
            assert_eq!(
                fs::metadata(&path.0).unwrap().len(),
                kept as u64,
                "corte em {cut}"
            );
        }
    }

    #[test]
    fn flipped_bit_at_any_byte_recovers_everything_before_its_record() {
        let recorded = record_scenario("flip");
        let path = TempPath::new("flip");

        for position in 0..recorded.bytes.len() {
            let mut bytes = recorded.bytes.clone();
            bytes[position] ^= 1 << (position % 8);
            let intact = complete_before(&recorded.ends, position);

            let state = boot_from(&path, &bytes);

            assert!(
                state == recorded.expected[intact],
                "bit trocado no byte {position} deveria recuperar {intact} comandos"
            );
        }
    }

    #[test]
    fn zeroed_tail_recovers_the_whole_log() {
        let recorded = record_scenario("zeros");
        let path = TempPath::new("zeros");
        let mut bytes = recorded.bytes.clone();
        bytes.extend_from_slice(&[0; 4096]);

        let state = boot_from(&path, &bytes);

        assert_eq!(state, *recorded.expected.last().unwrap());
    }

    // Sem o observador, o feed voltaria vazio de um reinício enquanto o engine voltaria
    // com o livro cheio — e o primeiro fill contra uma ordem restaurada derrubaria o feed.
    #[test]
    fn replay_observer_rebuilds_the_feed_after_a_restart() {
        let path = TempPath::new("feed-restart");
        {
            let (mut sequencer, _) = Sequencer::open(&path.0).unwrap();
            for intent in [
                fund(BRL, 100_000_000),
                fund(TESTE, 100_000),
                limit(Side::Ask, 10_000, 5, TimeInForce::Gtc),
                limit(Side::Ask, 10_100, 2, TimeInForce::Gtc),
                limit(Side::Bid, 9_900, 3, TimeInForce::Gtc),
            ] {
                sequencer.submit(intent).unwrap();
            }
        }

        let mut feed = market::new_market_data();
        let (mut sequencer, _) = Sequencer::open_observed(&path.0, |events| {
            feed.apply(events);
        })
        .unwrap();
        assert_feed_matches_engine(&feed, &sequencer);

        let events = sequencer
            .submit(limit(Side::Bid, 10_000, 2, TimeInForce::Gtc))
            .unwrap();
        feed.apply(&events);
        assert_feed_matches_engine(&feed, &sequencer);
    }

    fn assert_feed_matches_engine(feed: &MarketData, sequencer: &Sequencer) {
        let snapshot = feed.snapshot(TESTE_BRL, Feed::L2).unwrap();
        let book = sequencer.engine().book(TESTE_BRL).unwrap();
        let depth = |side| {
            book.depth(side)
                .into_iter()
                .map(|(price, lots)| Quote { price, lots })
                .collect::<Vec<_>>()
        };
        assert_eq!(snapshot.bids, depth(Side::Bid));
        assert_eq!(snapshot.asks, depth(Side::Ask));
    }

    #[test]
    fn counter_exhausted_mid_batch_changes_nothing() {
        let mut sequencer = Sequencer::in_memory();
        sequencer.counters.next_seq = u64::MAX - 1;
        let before = sequencer.counters;

        let result = sequencer.submit_batch(&[fund(BRL, 100), fund(BRL, 200)]);

        assert!(matches!(result, Err(SequencerError::CounterExhausted)));
        assert_eq!(sequencer.counters, before);
        assert_eq!(
            sequencer.balance(ACCOUNT, BRL),
            market::new_engine().balance(ACCOUNT, BRL)
        );
    }

    #[test]
    fn empty_batch_answers_nothing_and_logs_nothing() {
        let path = TempPath::new("empty-batch");
        let (mut sequencer, _) = Sequencer::open(&path.0).unwrap();

        assert_eq!(
            sequencer.submit_batch(&[]).unwrap(),
            Vec::<Vec<Event>>::new()
        );
        assert_eq!(sequencer.counters, Counters::FRESH);
        assert_eq!(fs::metadata(&path.0).unwrap().len(), 0);
    }

    fn intent() -> impl Strategy<Value = Intent> {
        let side = prop_oneof![Just(Side::Bid), Just(Side::Ask)];
        let tif = prop_oneof![
            Just(TimeInForce::Gtc),
            Just(TimeInForce::Ioc),
            Just(TimeInForce::Fok)
        ];
        prop_oneof![
            (1..10_000_000i64).prop_map(|amount| fund(BRL, amount)),
            (1..10_000i64).prop_map(|amount| fund(TESTE, amount)),
            (side, 95..=105i64, 1..6i64, tif).prop_map(|(side, hundreds, lots, tif)| limit(
                side,
                hundreds * 100,
                lots,
                tif
            )),
            (1..30u64).prop_map(cancel),
        ]
    }

    /// `intents` em pedaços com os tamanhos de `sizes`, repetidos em ciclo.
    fn chunks<'a>(mut intents: &'a [Intent], sizes: &[usize]) -> Vec<&'a [Intent]> {
        let mut chunks = Vec::new();
        for &size in sizes.iter().cycle() {
            if intents.is_empty() {
                break;
            }
            let (chunk, rest) = intents.split_at(size.min(intents.len()));
            chunks.push(chunk);
            intents = rest;
        }
        chunks
    }

    proptest! {
        // O lote só pode mudar quantos `fsync` acontecem: eventos, estado, contadores e os
        // bytes do log têm de ser exatamente os de uma submissão por vez.
        #[test]
        fn batches_are_indistinguishable_from_one_submit_per_intent(
            intents in prop::collection::vec(intent(), 0..40),
            sizes in prop::collection::vec(1..8usize, 1..10),
        ) {
            let one_by_one = TempPath::new("equivalence-one-by-one");
            let batched = TempPath::new("equivalence-batched");
            let (mut single, _) = Sequencer::open(&one_by_one.0).unwrap();
            let (mut grouped, _) = Sequencer::open(&batched.0).unwrap();

            let expected: Vec<Vec<Event>> =
                intents.iter().map(|&intent| single.submit(intent).unwrap()).collect();
            let answered: Vec<Vec<Event>> = chunks(&intents, &sizes)
                .into_iter()
                .flat_map(|chunk| grouped.submit_batch(chunk).unwrap())
                .collect();

            prop_assert_eq!(answered, expected);
            prop_assert_eq!(&grouped.engine, &single.engine);
            prop_assert_eq!(grouped.counters, single.counters);
            drop((single, grouped));
            prop_assert_eq!(fs::read(&batched.0).unwrap(), fs::read(&one_by_one.0).unwrap());
        }
    }

    // Os dois livros num log só, intercalados: o replay devolve cada ordem ao seu livro,
    // e o estado reconstruído é o mesmo de quem nunca reiniciou.
    #[test]
    fn both_books_survive_a_restart() {
        let intents = [
            fund(BRL, 100_000_000),
            fund(TESTE, 100_000),
            fund(OUTRO, 10_000),
            limit_on(TESTE_BRL, Side::Ask, 10_000, 5),
            limit_on(OUTRO_BRL, Side::Ask, 2_005, 3),
            limit_on(TESTE_BRL, Side::Bid, 9_900, 4),
            limit_on(OUTRO_BRL, Side::Bid, 2_005, 1),
            limit_on(OUTRO_BRL, Side::Bid, 1_995, 2),
        ];
        let path = TempPath::new("two-books");
        let mut live = Sequencer::in_memory();
        {
            let (mut durable, _) = Sequencer::open(&path.0).unwrap();
            for intent in intents {
                assert_eq!(
                    durable.submit(intent).unwrap(),
                    live.submit(intent).unwrap()
                );
            }
        }

        let (restarted, report) = Sequencer::open(&path.0).unwrap();

        assert_eq!(report.replayed, intents.len());
        assert_eq!(restarted.engine(), live.engine());
        let depth = |instrument, side| restarted.engine().book(instrument).unwrap().depth(side);
        assert_eq!(depth(TESTE_BRL, Side::Ask), [(price(10_000), lots(5))]);
        assert_eq!(depth(TESTE_BRL, Side::Bid), [(price(9_900), lots(4))]);
        assert_eq!(depth(OUTRO_BRL, Side::Ask), [(price(2_005), lots(2))]);
        assert_eq!(depth(OUTRO_BRL, Side::Bid), [(price(1_995), lots(2))]);
    }

    // Um ledger para todos os livros (decisão 10b): o BRL travado por uma compra num livro
    // não está disponível para comprar no outro.
    #[test]
    fn quote_reserved_in_one_book_is_not_available_to_the_other() {
        let mut sequencer = Sequencer::in_memory();
        sequencer.submit(fund(BRL, 10_000)).unwrap();
        sequencer
            .submit(limit_on(TESTE_BRL, Side::Bid, 3_000, 3))
            .unwrap();

        let events = sequencer
            .submit(limit_on(OUTRO_BRL, Side::Bid, 2_000, 1))
            .unwrap();

        assert!(matches!(
            events.as_slice(),
            [Event::Rejected {
                reason: RejectReason::InsufficientFunds { asset, .. },
                ..
            }] if *asset == BRL
        ));
    }

    fn price(value: i64) -> Price {
        Price::new(value).unwrap()
    }

    fn lots(value: i64) -> Lots {
        Lots::new(value).unwrap()
    }
}
