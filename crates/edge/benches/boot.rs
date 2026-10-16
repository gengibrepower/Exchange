use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use edge::market::{BRL, INSTRUMENTS, TESTE};
use edge::sequencer::{BootReport, Intent, Sequencer, SnapshotBoot, snapshot_path};
use exchange_core::domain::{
    AccountId, Amount, Lots, OrderId, OrderKind, Price, Side, TimeInForce,
};
use exchange_core::engine::Event;
use exchange_wal::{Wal, write_snapshot};

const LOGS: [usize; 2] = [100_000, 1_000_000];
const ACCOUNTS: u64 = 16;
const BATCH: usize = 1_000;
const RUNS: usize = 5;
const SEED: u64 = 0x5eed_b007;
// Nenhum múltiplo é alcançado: o único snapshot é o que o boot grava ao reaplicar algo.
const ONLY_AT_BOOT: Option<NonZeroU64> = NonZeroU64::new(u64::MAX);

fn main() {
    for commands in LOGS {
        let flow = order_flow(commands);
        let log = fresh_log(&format!("boot-{commands}"));

        let rejected = write_log(&log, &flow, None);
        let full = measure(|| boot(&log));
        let log_bytes = fs::metadata(&log).expect("metadados do log").len();

        drop(Sequencer::open(&log, ONLY_AT_BOOT).expect("gravar o snapshot no boot"));
        let snapshot_bytes = fs::metadata(snapshot_path(&log))
            .expect("metadados do snapshot")
            .len();
        let no_tail = measure(|| boot(&log));

        remove_log(&log);
        let tail = commands / 100;
        let (head, rest) = flow.split_at(commands - tail);
        write_log(&log, head, None);
        drop(Sequencer::open(&log, ONLY_AT_BOOT).expect("gravar o snapshot no boot"));
        write_log(&log, rest, ONLY_AT_BOOT);
        let with_tail = measure(|| boot(&log));
        let read_only = measure(|| Wal::open(&log).expect("ler o log"));

        let (sequencer, _) = boot(&log);
        let state = measure(|| sequencer.engine_state().to_bytes());
        let open_orders: usize = sequencer
            .engine_state()
            .instruments
            .iter()
            .map(|instrument| instrument.bids.len() + instrument.asks.len())
            .sum();
        let bytes = sequencer.engine_state().to_bytes();
        let scratch = log.with_extension("scratch");
        let write = measure(|| write_snapshot(&scratch, &bytes).expect("gravar o snapshot"));
        remove_log(&log);
        remove_if_present(&scratch);

        println!(
            "{commands} comandos ({}% recusados, {open_orders} ordens abertas no fim): \
             log de {}, snapshot de {}",
            rejected * 100 / commands,
            megabytes(log_bytes),
            megabytes(snapshot_bytes)
        );
        println!("  replay completo        {full}");
        println!("  snapshot, sem cauda    {no_tail}");
        println!("  snapshot + 1% do log   {with_tail}");
        println!("    só ler o log         {read_only}");
        println!("  montar o estado        {state}");
        println!("  gravar o snapshot      {write}");
    }
}

/// Mediana e pior caso de [`RUNS`] execuções.
struct Timing {
    median: Duration,
    worst: Duration,
}

impl std::fmt::Display for Timing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mediana {:>9.2?}  pior {:>9.2?}",
            self.median, self.worst
        )
    }
}

fn measure<T>(mut run: impl FnMut() -> T) -> Timing {
    let mut samples: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(run());
            start.elapsed()
        })
        .collect();
    samples.sort_unstable();
    Timing {
        median: samples[RUNS / 2],
        worst: samples[RUNS - 1],
    }
}

fn boot(log: &Path) -> (Sequencer, BootReport) {
    let booted = Sequencer::open(log, None).expect("subir do log");
    assert!(
        !matches!(booted.1.snapshot, SnapshotBoot::Discarded(_)),
        "o snapshot do próprio bench foi descartado"
    );
    booted
}

/// Grava `flow` em lotes de [`BATCH`], um `fsync` por lote: com um por comando, montar o
/// log levaria horas. Devolve quantas ordens o núcleo recusou.
fn write_log(log: &Path, flow: &[Intent], snapshot_every: Option<NonZeroU64>) -> usize {
    let (mut sequencer, _) = Sequencer::open(log, snapshot_every).expect("abrir o log");
    flow.chunks(BATCH)
        .flat_map(|chunk| sequencer.submit_batch(chunk).expect("gravar o lote"))
        .filter(|events| matches!(events.as_slice(), [Event::Rejected { .. }]))
        .count()
}

/// Depósitos para todas as contas e depois ordens limite em torno de 100,00 no TESTE/BRL:
/// 45% de cada lado, com um quinto delas cruzando o spread, e 10% de cancelamentos de uma
/// ordem anterior da mesma conta. O livro termina com muitas ordens, que é o que pesa no
/// snapshot.
fn order_flow(commands: usize) -> Vec<Intent> {
    let mut random = Lcg(SEED);
    let mut flow = Vec::with_capacity(commands);
    for number in 1..=ACCOUNTS {
        let account = AccountId::new(number);
        for (asset, amount) in [(BRL, 1_000_000_000_000_000), (TESTE, 10_000_000_000_000)] {
            let amount = Amount::new(amount).expect("quantia positiva");
            flow.push(Intent::Deposit {
                account,
                asset,
                amount,
            });
        }
    }
    let mut owners = Vec::new();
    while flow.len() < commands {
        let account = AccountId::new(1 + random.below(ACCOUNTS));
        let draw = random.below(100);
        if draw < 10 && !owners.is_empty() {
            let placed = u64::try_from(owners.len()).expect("ordens cabem em u64");
            let index = random.below(placed);
            flow.push(Intent::Cancel {
                account: owners[usize::try_from(index).expect("índice de uma ordem já posta")],
                instrument: INSTRUMENTS[0].1,
                order: OrderId::new(index + 1),
            });
            continue;
        }
        let side = if draw < 55 { Side::Bid } else { Side::Ask };
        let ticks = i64::try_from(1 + random.below(50)).expect("até 50 ticks");
        let crossing = random.below(5) == 0;
        let away = if crossing { -ticks } else { ticks };
        let offset = match side {
            Side::Bid => -away,
            Side::Ask => away,
        };
        let lots = i64::try_from(1 + random.below(10)).expect("até 10 lots");
        flow.push(Intent::Order {
            account,
            instrument: INSTRUMENTS[0].1,
            side,
            kind: OrderKind::Limit(Price::new(10_000 + offset * 10).expect("preço positivo")),
            lots: Lots::new(lots).expect("lots positivos"),
            time_in_force: TimeInForce::Gtc,
        });
        owners.push(account);
    }
    flow
}

/// Gerador congruencial linear (constantes de Knuth): o mesmo fluxo em toda rodada.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{}.{:02} MB", bytes / 1_000_000, bytes % 1_000_000 / 10_000)
}

/// Em `target/tmp`, no disco de verdade: num `tmpfs` o `fsync` não faz nada.
fn fresh_log(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("bench-{name}.wal"));
    remove_log(&path);
    path
}

fn remove_log(log: &Path) {
    remove_if_present(log);
    remove_if_present(&snapshot_path(log));
}

fn remove_if_present(path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            panic!("não deu para limpar {}: {error}", path.display())
        }
        _ => {}
    }
}
