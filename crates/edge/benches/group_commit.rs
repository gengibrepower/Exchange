use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use edge::core_thread::{CoreHandle, spawn_core};
use edge::market::{self, BRL};
use edge::sequencer::{Intent, Sequencer, SequencerError};
use exchange_core::domain::{AccountId, Amount};

const COMMANDS: u32 = 2_048;
const SESSIONS: [u32; 3] = [1, 8, 64];
const QUEUE: NonZeroUsize = NonZeroUsize::new(1_024).expect("1024 não é zero");
const FEED: NonZeroUsize = NonZeroUsize::new(4_096).expect("4096 não é zero");
const REPORTS: NonZeroUsize = NonZeroUsize::new(1_024).expect("1024 não é zero");

type CoreThread = JoinHandle<Result<Sequencer, SequencerError>>;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("criar o runtime");

    for sessions in SESSIONS {
        let (memory, core) = runtime.block_on(measure(Sequencer::in_memory(), sessions));
        stop(core);

        let log = fresh_log(&format!("group-commit-{sessions}"));
        let (sequencer, _) = Sequencer::open(&log).expect("abrir o WAL");
        let (durable, core) = runtime.block_on(measure(sequencer, sessions));
        stop(core);
        remove_if_present(&log);

        println!(
            "{sessions:>2} sessões: memória {:>9.0} cmd/s   WAL {:>7.0} cmd/s",
            per_second(memory),
            per_second(durable),
        );
    }
}

/// O tempo de parede para `sessions` clientes concorrentes, juntos, terem [`COMMANDS`]
/// comandos respondidos.
async fn measure(sequencer: Sequencer, sessions: u32) -> (Duration, CoreThread) {
    let (core, thread) = spawn_core(sequencer, market::new_market_data(), QUEUE, FEED, REPORTS)
        .expect("criar a thread do núcleo");
    let per_session = COMMANDS / sessions;

    let start = Instant::now();
    let tasks: Vec<_> = (1..=sessions)
        .map(|account| {
            let account = AccountId::new(u64::from(account));
            tokio::spawn(session(core.clone(), account, per_session))
        })
        .collect();
    for task in tasks {
        task.await.expect("sessão sem panic");
    }
    (start.elapsed(), thread)
}

/// Um cliente em closed loop: manda `commands` depósitos, um de cada vez, e só manda o
/// próximo depois da resposta do anterior.
async fn session(core: CoreHandle, account: AccountId, commands: u32) {
    for _ in 0..commands {
        core.submit(deposit(account))
            .await
            .expect("o nucleo nao respondeu");
    }
}

/// O comando mais barato para o núcleo e sempre aceito: o que se mede é o caminho até o
/// disco e de volta, não o matching.
fn deposit(account: AccountId) -> Intent {
    Intent::Deposit {
        account,
        asset: BRL,
        amount: Amount::new(1).expect("1 é positivo"),
    }
}

/// Com o último `CoreHandle` já descartado, o núcleo esvazia a fila e termina.
fn stop(core: CoreThread) {
    core.join()
        .expect("núcleo sem panic")
        .expect("núcleo sem erro");
}

fn per_second(elapsed: Duration) -> f64 {
    f64::from(COMMANDS) / elapsed.as_secs_f64()
}

/// Em `target/tmp`, no disco de verdade: num `tmpfs` o `fsync` não faz nada.
fn fresh_log(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("bench-{name}.wal"));
    remove_if_present(&path);
    path
}

fn remove_if_present(path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            panic!("não deu para limpar {}: {error}", path.display())
        }
        _ => {}
    }
}
