use std::hint::black_box;
use std::time::{Duration, Instant};

mod support;

const ORDERS: usize = 100_000;

fn main() {
    for (name, mix) in support::SCENARIOS {
        let flow = support::order_flow(support::SEED, mix, ORDERS);
        support::check_flow(&flow);

        let mut engine = support::funded_engine();
        let mut samples = Vec::with_capacity(flow.len());
        for command in flow {
            let start = Instant::now();
            let events = engine.apply(command);
            samples.push(start.elapsed());
            black_box(events);
        }
        samples.sort_unstable();

        println!(
            "{name:>13}: p50 {:>9.2?}  p99 {:>9.2?}  p99.9 {:>9.2?}  max {:>9.2?}",
            percentile(&samples, 500),
            percentile(&samples, 990),
            percentile(&samples, 999),
            percentile(&samples, 1_000),
        );
    }
}

/// O menor valor de `sorted` que cobre `permille` milésimos das amostras
/// (`500` = mediana, `990` = p99, `1000` = máximo).
fn percentile(sorted: &[Duration], permille: usize) -> Duration {
    let rank = (sorted.len() * permille).div_ceil(1000);
    sorted
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or_default()
}
