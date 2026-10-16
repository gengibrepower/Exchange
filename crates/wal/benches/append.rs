use std::fs::{File, OpenOptions};
use std::hint::black_box;
use std::io::Write;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use exchange_wal::{HEADER_LEN, Wal, encode};

mod support;

use support::{BenchLog, PAYLOAD};

const BATCHES: [usize; 3] = [1, 8, 64];

fn per_record(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal");
    group.throughput(Throughput::Elements(1));

    let log = BenchLog::new("append");
    let mut wal = Wal::open(log.path()).expect("abrir o WAL").wal;
    group.bench_function("append", |b| {
        b.iter(|| wal.append(black_box(&PAYLOAD)).expect("gravar no WAL"));
    });

    let log = BenchLog::new("no_fsync");
    let mut file = open_raw(&log);
    group.bench_function("no_fsync", |b| {
        b.iter(|| {
            let mut frame = Vec::with_capacity(HEADER_LEN + PAYLOAD.len());
            encode(black_box(&PAYLOAD), &mut frame).expect("payload pequeno");
            file.write_all(&frame).expect("gravar no arquivo");
        });
    });

    for records in BATCHES {
        let log = BenchLog::new(&format!("batch-{records}"));
        let mut wal = Wal::open(log.path()).expect("abrir o WAL").wal;
        let batch = vec![&PAYLOAD[..]; records];
        group.throughput(Throughput::Elements(records as u64));
        group.bench_function(format!("batch/{records}"), |b| {
            b.iter(|| wal.append_batch(black_box(&batch)).expect("gravar o lote"));
        });
    }

    group.finish();
}

/// O arquivo cru, sem a API do `Wal`: o `no_fsync` não é uma operação que o WAL ofereça.
fn open_raw(log: &BenchLog) -> File {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(log.path())
        .expect("abrir o log do benchmark")
}

criterion_group!(benches, per_record);
criterion_main!(benches);
