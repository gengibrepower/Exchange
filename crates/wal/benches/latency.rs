use std::hint::black_box;
use std::time::{Duration, Instant};

use exchange_wal::Wal;

mod support;

use support::{BenchLog, PAYLOAD};

const APPENDS: usize = 5_000;

fn main() {
    let log = BenchLog::new("latency");
    let mut wal = Wal::open(log.path()).expect("abrir o WAL").wal;

    let mut samples = Vec::with_capacity(APPENDS);
    for _ in 0..APPENDS {
        let start = Instant::now();
        wal.append(black_box(&PAYLOAD)).expect("gravar no WAL");
        samples.push(start.elapsed());
    }
    samples.sort_unstable();

    println!(
        "append: p50 {:>9.2?}  p99 {:>9.2?}  p99.9 {:>9.2?}  max {:>9.2?}",
        percentile(&samples, 500),
        percentile(&samples, 990),
        percentile(&samples, 999),
        percentile(&samples, 1_000),
    );
}

/// O menor valor de `sorted` que cobre `permille` milésimos das amostras
/// (`500` = mediana, `990` = p99, `1000` = máximo). Cópia da do `exchange-core`: cada
/// benchmark é um crate à parte e não há crate de suporte compartilhado.
fn percentile(sorted: &[Duration], permille: usize) -> Duration {
    let rank = (sorted.len() * permille).div_ceil(1000);
    sorted
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or_default()
}
