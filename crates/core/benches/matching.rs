use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

mod support;

const ORDERS: usize = 10_000;

fn engine_apply(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine_apply");
    for (name, mix) in support::SCENARIOS {
        let flow = support::order_flow(support::SEED, mix, ORDERS);
        support::check_flow(&flow);
        group.throughput(Throughput::Elements(flow.len() as u64));
        group.bench_function(name, |b| {
            b.iter_batched(
                || (support::funded_engine(), flow.clone()),
                |(mut engine, flow)| {
                    for command in flow {
                        black_box(engine.apply(command));
                    }
                    engine
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, engine_apply);
criterion_main!(benches);
