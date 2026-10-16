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
        let mut file = open_raw(&log);
        group.throughput(Throughput::Elements(records as u64));
        group.bench_function(format!("batch/{records}"), |b| {
            b.iter(|| write_batch(&mut file, black_box(records)));
        });
    }

    group.finish();
}

/// O arquivo cru, sem a API do `Wal`, para os cenários que ele ainda não tem.
fn open_raw(log: &BenchLog) -> File {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(log.path())
        .expect("abrir o log do benchmark")
}

/// Simulação de group commit: `records` frames num buffer só, um `write_all` e um
/// `sync_data` para todos. Ao voltar, todos sobrevivem a um crash.
fn write_batch(file: &mut File, records: usize) {
    let mut buffer = Vec::with_capacity(records * (HEADER_LEN + PAYLOAD.len()));
    for _ in 0..records {
        encode(&PAYLOAD, &mut buffer).expect("payload pequeno");
    }
    file.write_all(&buffer).expect("gravar no arquivo");
    file.sync_data().expect("sincronizar com disco");
}

criterion_group!(benches, per_record);
criterion_main!(benches);
