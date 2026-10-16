use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use exchange_core::domain::{
    AccountId, Amount, AssetId, InstrumentId, Lots, OrderId, OrderKind, OrderRequest, Seq, Side,
    TimeInForce,
};
use exchange_core::engine::{Command, Engine, EngineState, Event, RestoreError, StateDecodeError};
use exchange_core::ledger::Balance;
use exchange_core::record::{LogRecord, RecordError};
use exchange_wal::{SnapshotError, Wal, WalError, read_snapshot, write_snapshot};

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
    /// O snapshot cobre mais registros do que o log tem. O log é gravado antes de aplicar
    /// e o snapshot só cobre o que já foi aplicado, então isso não vem de crash: o log
    /// foi trocado ou perdido, e o boot não tem como saber qual dos dois é a verdade.
    SnapshotAhead {
        covered: u64,
        logged: usize,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::UnexpectedSeq { expected, found } => {
                write!(f, "seq {found} onde se esperava {expected}")
            }
            Self::SeqExhausted => write!(f, "seq chegou ao limite de u64"),
            Self::SnapshotAhead { covered, logged } => write!(
                f,
                "o snapshot cobre {covered} comandos e o log só tem {logged}"
            ),
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

#[derive(Debug)]
pub struct BootReport {
    /// Comandos do log reaplicados: só os depois do snapshot, se houve um.
    pub replayed: usize,
    pub discarded: u64,
    pub snapshot: SnapshotBoot,
}

#[derive(Debug)]
pub enum SnapshotBoot {
    Missing,
    /// O estado depois dos `covered` primeiros comandos do log.
    Loaded {
        covered: u64,
    },
    /// Não deu para usar o snapshot e o log foi reaplicado inteiro: ele é só um atalho, e
    /// o log continua sendo a verdade.
    Discarded(SnapshotProblem),
}

impl fmt::Display for SnapshotBoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(f, "nenhum"),
            Self::Loaded { covered } => write!(f, "carregado, cobre {covered} comandos"),
            Self::Discarded(problem) => write!(f, "descartado ({problem}), log reaplicado inteiro"),
        }
    }
}

#[derive(Debug)]
pub enum SnapshotProblem {
    File(SnapshotError),
    /// Contadores que nenhum sequencer gravaria: curtos demais ou zerados.
    Counters,
    State(StateDecodeError),
    /// O estado não vale na configuração de agora, que pode ter mudado desde a gravação.
    Restore(RestoreError),
}

impl fmt::Display for SnapshotProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(error) => write!(f, "{error}"),
            Self::Counters => write!(f, "contadores inválidos"),
            Self::State(error) => write!(f, "{error}"),
            Self::Restore(error) => write!(f, "estado recusado: {error:?}"),
        }
    }
}

/// Onde e de quantos em quantos comandos o sequencer grava o snapshot.
struct Snapshots {
    path: PathBuf,
    every: NonZeroU64,
}

/// O núcleo rodando: único dono do engine, do WAL e dos contadores (single writer). Todo
/// comando passa por [`Sequencer::submit_batch`], que grava antes de aplicar (seção 4.6).
pub struct Sequencer {
    engine: Engine,
    wal: Option<Wal>,
    counters: Counters,
    snapshots: Option<Snapshots>,
}

impl Sequencer {
    pub fn in_memory() -> Self {
        Self {
            engine: market::new_engine(),
            wal: None,
            counters: Counters::FRESH,
            snapshots: None,
        }
    }

    /// Abre o WAL em `path` (cortando rabo rasgado) e sobe antes de aceitar qualquer
    /// comando novo: do snapshot em [`snapshot_path`], se houver um utilizável, mais o
    /// resto do log; sem ele, do log inteiro. Com `snapshot_every`, grava um snapshot a
    /// cada tantos comandos, e um logo no boot se algo foi reaplicado.
    ///
    /// # Errors
    /// [`SequencerError::Wal`] se o arquivo não puder ser aberto ou recuperado;
    /// [`SequencerError::Replay`] se um registro íntegro não puder ser reaplicado ou se o
    /// snapshot estiver à frente do log.
    pub fn open(
        path: &Path,
        snapshot_every: Option<NonZeroU64>,
    ) -> Result<(Self, BootReport), SequencerError> {
        let recovery = Wal::open(path)?;
        let snapshot = snapshot_path(path);
        let (counters, mut engine, boot) = match load_snapshot(&snapshot) {
            Ok(Some((counters, engine))) => {
                let covered = counters.next_seq - 1;
                (counters, engine, SnapshotBoot::Loaded { covered })
            }
            Ok(None) => (Counters::FRESH, market::new_engine(), SnapshotBoot::Missing),
            Err(problem) => (
                Counters::FRESH,
                market::new_engine(),
                SnapshotBoot::Discarded(problem),
            ),
        };
        let covered = counters.next_seq - 1;
        let tail = usize::try_from(covered)
            .ok()
            .and_then(|covered| recovery.records.get(covered..))
            .ok_or(ReplayError::SnapshotAhead {
                covered,
                logged: recovery.records.len(),
            })
            .map_err(SequencerError::Replay)?;
        let counters = replay(&mut engine, tail, counters).map_err(SequencerError::Replay)?;
        let report = BootReport {
            replayed: tail.len(),
            discarded: recovery.discarded,
            snapshot: boot,
        };
        let sequencer = Self {
            engine,
            wal: Some(recovery.wal),
            counters,
            snapshots: snapshot_every.map(|every| Snapshots {
                path: snapshot,
                every,
            }),
        };
        if report.replayed > 0 {
            sequencer.save_snapshot();
        }
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
        let applied_before = self.counters.next_seq - 1;
        self.counters = counters;
        let results = records
            .iter()
            .map(|record| self.engine.apply(record.command()))
            .collect();
        if let Some(snapshots) = &self.snapshots {
            let every = snapshots.every.get();
            if (counters.next_seq - 1) / every > applied_before / every {
                self.save_snapshot();
            }
        }
        Ok(results)
    }

    /// Grava o snapshot do estado de agora, se este sequencer grava snapshots. Falhar aqui
    /// não para o núcleo, ao contrário do WAL: o log está íntegro e só se perde o atalho
    /// até a próxima tentativa.
    fn save_snapshot(&self) {
        let Some(snapshots) = &self.snapshots else {
            return;
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&self.counters.next_seq.to_le_bytes());
        payload.extend_from_slice(&self.counters.next_order_id.to_le_bytes());
        payload.extend_from_slice(&self.engine.state().to_bytes());
        if let Err(error) = write_snapshot(&snapshots.path, &payload) {
            eprintln!("aviso: snapshot não gravado: {error}");
        }
    }

    /// O estado do engine, de onde o feed de market data se reconstrói no boot.
    pub fn engine_state(&self) -> EngineState {
        self.engine.state()
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

/// O snapshot mora ao lado do log que ele resume: `<wal>.snapshot`.
pub fn snapshot_path(wal: &Path) -> PathBuf {
    let mut name = OsString::from(wal.as_os_str());
    name.push(".snapshot");
    PathBuf::from(name)
}

const COUNTERS_LEN: usize = 16;

/// O snapshot é `[next_seq][next_order_id][estado]`, inteiros em little-endian. O id de
/// ordem vai junto porque não sai do estado: as ordens já executadas não estão mais nele.
fn load_snapshot(path: &Path) -> Result<Option<(Counters, Engine)>, SnapshotProblem> {
    let Some(bytes) = read_snapshot(path).map_err(SnapshotProblem::File)? else {
        return Ok(None);
    };
    let (counters, state) = bytes
        .split_first_chunk::<COUNTERS_LEN>()
        .ok_or(SnapshotProblem::Counters)?;
    let (next_seq, next_order_id) = counters.split_at(COUNTERS_LEN / 2);
    let counter = |bytes: &[u8]| {
        bytes
            .try_into()
            .ok()
            .map(u64::from_le_bytes)
            .filter(|&counter| counter > 0)
            .ok_or(SnapshotProblem::Counters)
    };
    let counters = Counters {
        next_seq: counter(next_seq)?,
        next_order_id: counter(next_order_id)?,
    };
    let state = EngineState::from_bytes(state).map_err(SnapshotProblem::State)?;
    let engine = market::restore_engine(&state).map_err(SnapshotProblem::Restore)?;
    Ok(Some((counters, engine)))
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

/// Reaplica no engine os registros que vêm depois de `counters`. Os eventos não vão para
/// ninguém: os clientes já os receberam na sessão que gravou cada comando. Os contadores
/// saem do próprio log.
fn replay(
    engine: &mut Engine,
    records: &[Vec<u8>],
    mut counters: Counters,
) -> Result<Counters, ReplayError> {
    for bytes in records {
        let record = LogRecord::from_bytes(bytes)?;
        counters.next_seq = check_seq(counters.next_seq, record.seq())?;
        let command = record.command();
        if let Command::Submit(request) = command {
            let after = request.id.get().saturating_add(1);
            counters.next_order_id = counters.next_order_id.max(after);
        }
        engine.apply(command);
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
    use exchange_core::engine::InstrumentState;

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
        let counters = replay(
            &mut market::new_engine(),
            &[deposit(1), deposit(2)],
            Counters::FRESH,
        )
        .unwrap();

        assert_eq!(counters.next_seq, 3);
    }

    #[test]
    fn replay_rejects_a_gap_in_seq() {
        let error = replay(
            &mut market::new_engine(),
            &[deposit(1), deposit(3)],
            Counters::FRESH,
        )
        .unwrap_err();

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
        let error = replay(
            &mut market::new_engine(),
            &[deposit(1), deposit(1)],
            Counters::FRESH,
        )
        .unwrap_err();

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

    fn stop(side: Side, trigger: i64, limit: Option<i64>, lots: i64) -> Intent {
        let price = |units| Price::new(units).unwrap();
        let kind = OrderKind::Stop {
            trigger: price(trigger),
            limit: limit.map(price),
        };
        order(side, kind, lots, TimeInForce::Gtc)
    }

    // Uma sessão que passa por todos os caminhos que mexem em estado: depósito,
    // descanso, fill parcial e total, IOC, cancelamento, rejeições, FOK expirada e stops,
    // um disparado e outro que fica dormente.
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
            limit(Ask, 10_300, 2, Gtc),
            stop(Bid, 10_300, None, 1),
            stop(Ask, 9_000, Some(8_900), 1),
            limit(Bid, 10_300, 1, Gtc),
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
        let (mut sequencer, _) = Sequencer::open(&path.0, None).unwrap();
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

    #[test]
    fn scenario_fires_one_stop_and_leaves_one_dormant() {
        let mut sequencer = Sequencer::in_memory();
        let events: Vec<_> = scenario()
            .into_iter()
            .flat_map(|intent| sequencer.submit(intent).unwrap())
            .collect();
        let count =
            |wanted: fn(&Event) -> bool| events.iter().filter(|event| wanted(event)).count();

        assert_eq!(count(|event| matches!(event, Event::Armed { .. })), 2);
        assert_eq!(count(|event| matches!(event, Event::Triggered { .. })), 1);
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
        let (sequencer, _) = Sequencer::open(&path.0, None).unwrap();
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

    // Um feed que voltasse vazio de um reinício, com o engine de livro cheio, cairia no
    // primeiro fill contra uma ordem restaurada: o maker não estaria em repouso nele.
    #[test]
    fn the_feed_rebuilt_from_the_state_follows_the_engine_after_a_restart() {
        let path = TempPath::new("feed-restart");
        {
            let (mut sequencer, _) = Sequencer::open(&path.0, None).unwrap();
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

        let (mut sequencer, _) = Sequencer::open(&path.0, None).unwrap();
        let mut feed = market::restore_market_data(&sequencer.engine_state());
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
        let (mut sequencer, _) = Sequencer::open(&path.0, None).unwrap();

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
            let (mut single, _) = Sequencer::open(&one_by_one.0, None).unwrap();
            let (mut grouped, _) = Sequencer::open(&batched.0, None).unwrap();

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
            let (mut durable, _) = Sequencer::open(&path.0, None).unwrap();
            for intent in intents {
                assert_eq!(
                    durable.submit(intent).unwrap(),
                    live.submit(intent).unwrap()
                );
            }
        }

        let (restarted, report) = Sequencer::open(&path.0, None).unwrap();

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

    fn every(commands: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(commands)
    }

    fn submit_all(path: &TempPath, snapshot_every: Option<NonZeroU64>, intents: &[Intent]) {
        let (mut sequencer, _) = Sequencer::open(&path.0, snapshot_every).unwrap();
        for &intent in intents {
            sequencer.submit(intent).unwrap();
        }
    }

    fn boot(path: &TempPath) -> (Sequencer, BootReport) {
        Sequencer::open(&path.0, None).unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // Decisão 6: subir do snapshot mais o resto do log é indistinguível de reaplicar o
        // log inteiro: mesmo engine, mesmos contadores e mesma resposta ao próximo comando.
        // Em lotes, para o múltiplo de `every` cair no meio de um.
        #[test]
        fn booting_from_a_snapshot_is_the_same_as_replaying_the_whole_log(
            intents in prop::collection::vec(intent(), 0..40),
            sizes in prop::collection::vec(1..5usize, 1..6),
            commands in 1..12u64,
        ) {
            let snapshotted = TempPath::new("snapshot-equivalence");
            let replayed = TempPath::new("snapshot-equivalence-replayed");
            {
                let (mut sequencer, _) = Sequencer::open(&snapshotted.0, every(commands)).unwrap();
                for chunk in chunks(&intents, &sizes) {
                    sequencer.submit_batch(chunk).unwrap();
                }
            }
            fs::copy(&snapshotted.0, &replayed.0).unwrap();

            let (mut from_snapshot, report) = boot(&snapshotted);
            let (mut from_log, full) = boot(&replayed);

            prop_assert!(matches!(full.snapshot, SnapshotBoot::Missing));
            let covered = match report.snapshot {
                SnapshotBoot::Loaded { covered } => covered,
                SnapshotBoot::Missing => 0,
                SnapshotBoot::Discarded(problem) => panic!("snapshot descartado: {problem}"),
            };
            let logged = u64::try_from(intents.len()).unwrap();
            prop_assert_eq!(covered > 0, logged >= commands);
            prop_assert_eq!(covered + u64::try_from(report.replayed).unwrap(), logged);
            prop_assert_eq!(&from_snapshot.engine, &from_log.engine);
            prop_assert_eq!(from_snapshot.counters, from_log.counters);
            let next = limit(Side::Bid, 10_000, 1, TimeInForce::Gtc);
            prop_assert_eq!(from_snapshot.submit(next).unwrap(), from_log.submit(next).unwrap());
        }
    }

    fn history() -> Vec<Intent> {
        vec![
            fund(BRL, 100_000_000),
            fund(TESTE, 100_000),
            limit(Side::Ask, 10_000, 5, TimeInForce::Gtc),
            limit(Side::Bid, 9_900, 3, TimeInForce::Gtc),
            limit(Side::Bid, 10_000, 2, TimeInForce::Gtc),
        ]
    }

    /// Um log com a história e o estado esperado dela, sem snapshot nenhum ao lado.
    fn logged_history(name: &str) -> (TempPath, Engine) {
        let path = TempPath::new(name);
        submit_all(&path, None, &history());
        let expected = boot(&path).0.engine;
        (path, expected)
    }

    fn write_payload(path: &TempPath, payload: &[u8]) {
        write_snapshot(&snapshot_path(&path.0), payload).unwrap();
    }

    fn payload(next_seq: u64, next_order_id: u64, state: &[u8]) -> Vec<u8> {
        let mut bytes = next_seq.to_le_bytes().to_vec();
        bytes.extend_from_slice(&next_order_id.to_le_bytes());
        bytes.extend_from_slice(state);
        bytes
    }

    /// Cada problema com o snapshot cai no log inteiro, com o mesmo estado de quem nunca
    /// gravou snapshot.
    fn assert_discarded(name: &str, payload: &[u8], wanted: fn(&SnapshotProblem) -> bool) {
        let (path, expected) = logged_history(name);
        write_payload(&path, payload);

        let (sequencer, report) = boot(&path);

        let SnapshotBoot::Discarded(problem) = &report.snapshot else {
            panic!("snapshot usado: {:?}", report.snapshot);
        };
        assert!(wanted(problem), "{problem:?}");
        assert_eq!(report.replayed, history().len());
        assert_eq!(sequencer.engine, expected);
    }

    #[test]
    fn a_damaged_snapshot_falls_back_to_the_whole_log() {
        let (path, expected) = logged_history("snapshot-damaged");
        fs::write(snapshot_path(&path.0), b"EXSNAP01 cortado").unwrap();

        let (sequencer, report) = boot(&path);

        assert!(matches!(
            report.snapshot,
            SnapshotBoot::Discarded(SnapshotProblem::File(SnapshotError::Damaged))
        ));
        assert_eq!(sequencer.engine, expected);
    }

    #[test]
    fn snapshot_counters_must_be_present_and_positive() {
        let counters = |problem: &SnapshotProblem| matches!(problem, SnapshotProblem::Counters);
        let state = market::new_engine().state().to_bytes();
        assert_discarded("snapshot-short", b"curto", counters);
        assert_discarded("snapshot-seq-zero", &payload(0, 1, &state), counters);
        assert_discarded("snapshot-id-zero", &payload(1, 0, &state), counters);
    }

    #[test]
    fn a_state_that_does_not_decode_falls_back_to_the_whole_log() {
        assert_discarded("snapshot-garbage", &payload(3, 1, b"lixo"), |problem| {
            matches!(problem, SnapshotProblem::State(_))
        });
    }

    // Um instrumento que saiu da configuração desde a gravação, por exemplo.
    #[test]
    fn a_state_the_configuration_refuses_falls_back_to_the_whole_log() {
        let mut state = market::new_engine().state();
        state.instruments.push(InstrumentState {
            instrument: InstrumentId::new(99),
            last_trade: None,
            bids: Vec::new(),
            asks: Vec::new(),
            stops: Vec::new(),
        });
        assert_discarded(
            "snapshot-refused",
            &payload(3, 1, &state.to_bytes()),
            |problem| {
                matches!(
                    problem,
                    SnapshotProblem::Restore(RestoreError::UnknownInstrument(_))
                )
            },
        );
    }

    #[test]
    fn a_snapshot_ahead_of_the_log_stops_the_boot() {
        let path = TempPath::new("snapshot-ahead");
        submit_all(&path, every(1), &history());
        fs::write(&path.0, b"").unwrap();

        let error = Sequencer::open(&path.0, None).err().unwrap();

        assert!(matches!(
            error,
            SequencerError::Replay(ReplayError::SnapshotAhead {
                covered: 5,
                logged: 0
            })
        ));
    }

    // Quem passa a gravar snapshots num log antigo já ganha um no boot, sem esperar o
    // primeiro múltiplo.
    #[test]
    fn the_boot_writes_a_snapshot_when_it_replayed_something() {
        let (path, expected) = logged_history("snapshot-at-boot");
        drop(Sequencer::open(&path.0, every(100)).unwrap());

        let (sequencer, report) = boot(&path);

        assert!(matches!(
            report.snapshot,
            SnapshotBoot::Loaded { covered: 5 }
        ));
        assert_eq!(report.replayed, 0);
        assert_eq!(sequencer.engine, expected);
    }

    #[test]
    fn snapshots_are_written_only_at_multiples_of_the_interval() {
        let path = TempPath::new("snapshot-interval");
        submit_all(&path, every(2), &history());

        let (_, report) = boot(&path);

        assert!(matches!(
            report.snapshot,
            SnapshotBoot::Loaded { covered: 4 }
        ));
        assert_eq!(report.replayed, 1);
    }

    // Um diretório no lugar do temporário faz toda gravação de snapshot falhar; os
    // comandos continuam sendo aceitos, gravados no log e aplicados.
    #[test]
    fn a_snapshot_that_cannot_be_written_does_not_stop_the_sequencer() {
        let path = TempPath::new("snapshot-unwritable");
        let mut temporary = snapshot_path(&path.0).into_os_string();
        temporary.push(".tmp");
        fs::create_dir(&temporary).unwrap();

        submit_all(&path, every(1), &history());
        let (sequencer, report) = boot(&path);
        fs::remove_dir(&temporary).unwrap();

        assert!(matches!(report.snapshot, SnapshotBoot::Missing));
        assert_eq!(report.replayed, history().len());
        assert_eq!(sequencer.counters.next_seq, 6);
    }
}
