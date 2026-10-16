use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use edge::core_thread::spawn_core;
use edge::fix_gateway::{self, Registry};
use edge::market::{new_market_data, restore_market_data};
use edge::sequencer::{Sequencer, SnapshotBoot};
use edge::server::{Limits, serve};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const USAGE: &str = "uso: server [--listen <endereço:porta>] [--fix <endereço:porta>] \
                     [--wal <caminho> [--snapshot-every <comandos>]]";

// Só a própria máquina por padrão: um simulador sem autenticação não deve aparecer na rede
// por acidente (seção 6). Expor é pedir explicitamente com --listen.
const DEFAULT_LISTEN: &str = "127.0.0.1:7000";
const DEFAULT_FIX: &str = "127.0.0.1:7001";

const QUEUE: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 não é zero");

// Incrementos guardados para assinantes atrasados; quem ficar mais para trás que isso perde
// mensagens, vê o buraco na sequência e pede um snapshot novo.
const FEED: NonZeroUsize = NonZeroUsize::new(4096).expect("4096 não é zero");

// Entregas privadas guardadas por sessão; quem acumular mais que isso sem ler é desconectado.
const REPORTS: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 não é zero");

const LIMITS: Limits = Limits {
    max_connections: NonZeroUsize::new(1024).expect("1024 não é zero"),
    idle_timeout: Duration::from_mins(5),
};

#[derive(Debug, PartialEq, Eq)]
struct Args {
    listen: SocketAddr,
    fix: SocketAddr,
    wal: Option<PathBuf>,
    snapshot_every: Option<NonZeroU64>,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut listen = None;
    let mut fix = None;
    let mut wal = None;
    let mut snapshot_every = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let address = || {
            value
                .to_str()
                .and_then(|text| text.parse().ok())
                .ok_or(USAGE)
        };
        let slot_was_empty = if flag == "--listen" {
            listen.replace(address()?).is_none()
        } else if flag == "--fix" {
            fix.replace(address()?).is_none()
        } else if flag == "--wal" {
            wal.replace(PathBuf::from(value)).is_none()
        } else if flag == "--snapshot-every" {
            let every = value.to_str().and_then(|text| text.parse().ok());
            snapshot_every.replace(every.ok_or(USAGE)?).is_none()
        } else {
            return Err(USAGE);
        };
        if !slot_was_empty {
            return Err(USAGE);
        }
    }
    let listen = match listen {
        Some(address) => address,
        None => DEFAULT_LISTEN.parse().map_err(|_| USAGE)?,
    };
    let fix = match fix {
        Some(address) => address,
        None => DEFAULT_FIX.parse().map_err(|_| USAGE)?,
    };
    if wal.is_none() && snapshot_every.is_some() {
        return Err(USAGE);
    }
    Ok(Args {
        listen,
        fix,
        wal,
        snapshot_every,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("erro: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> io::Result<()> {
    let args = parse_args(std::env::args_os().skip(1)).map_err(io::Error::other)?;
    let (sequencer, market_data) = match &args.wal {
        None => (Sequencer::in_memory(), new_market_data()),
        Some(path) => {
            let (sequencer, report) =
                Sequencer::open(path, args.snapshot_every).map_err(io::Error::other)?;
            eprintln!(
                "log: {} comandos reaplicados, {} bytes descartados",
                report.replayed, report.discarded
            );
            if !matches!(report.snapshot, SnapshotBoot::Missing) {
                eprintln!("snapshot: {}", report.snapshot);
            }
            // O feed também é estado derivado: volta com o livro que o engine voltou.
            let market_data = restore_market_data(&sequencer.engine_state());
            (sequencer, market_data)
        }
    };
    let (core, core_thread) = spawn_core(sequencer, market_data, QUEUE, FEED, REPORTS)?;

    // O join da thread do núcleo bloqueia; ele mora numa thread comum e avisa por um
    // oneshot, para o runtime nunca ficar preso esperando.
    let (stopped, core_stopped) = oneshot::channel();
    thread::spawn(move || {
        let _ = stopped.send(core_thread.join());
    });

    let listener = TcpListener::bind(args.listen).await?;
    eprintln!("ouvindo em {}", listener.local_addr()?);
    let fix_listener = TcpListener::bind(args.fix).await?;
    eprintln!("FIX ouvindo em {}", fix_listener.local_addr()?);
    let registry = Arc::new(Registry::default());

    tokio::select! {
        served = serve(listener, core.clone(), LIMITS) => served,
        served = fix_gateway::serve(fix_listener, core, registry, LIMITS) => served,
        outcome = core_stopped => Err(io::Error::other(match outcome {
            Ok(Ok(Err(error))) => format!("o núcleo parou: {error}"),
            Ok(Err(_)) => "o núcleo entrou em pânico".to_string(),
            Ok(Ok(Ok(_))) | Err(_) => "o núcleo parou".to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Args, &'static str> {
        parse_args(list.iter().map(OsString::from))
    }

    #[test]
    fn defaults_to_loopback_in_memory() {
        assert_eq!(
            parse(&[]),
            Ok(Args {
                listen: "127.0.0.1:7000".parse().unwrap(),
                fix: "127.0.0.1:7001".parse().unwrap(),
                wal: None,
                snapshot_every: None,
            })
        );
    }

    #[test]
    fn flags_in_any_order() {
        let expected = Ok(Args {
            listen: "0.0.0.0:9000".parse().unwrap(),
            fix: "0.0.0.0:9001".parse().unwrap(),
            wal: Some(PathBuf::from("x.wal")),
            snapshot_every: NonZeroU64::new(100),
        });
        assert_eq!(
            parse(&[
                "--listen",
                "0.0.0.0:9000",
                "--fix",
                "0.0.0.0:9001",
                "--wal",
                "x.wal",
                "--snapshot-every",
                "100"
            ]),
            expected
        );
        assert_eq!(
            parse(&[
                "--snapshot-every",
                "100",
                "--wal",
                "x.wal",
                "--fix",
                "0.0.0.0:9001",
                "--listen",
                "0.0.0.0:9000"
            ]),
            expected
        );
    }

    #[test]
    fn bad_arguments_show_usage() {
        assert_eq!(parse(&["--listen"]), Err(USAGE));
        assert_eq!(parse(&["--listen", "localhost"]), Err(USAGE));
        assert_eq!(parse(&["--port", "7000"]), Err(USAGE));
        assert_eq!(parse(&["--wal", "a", "--wal", "b"]), Err(USAGE));
        assert_eq!(
            parse(&["--fix", "1.2.3.4:5", "--fix", "1.2.3.4:6"]),
            Err(USAGE)
        );
        assert_eq!(parse(&["--fix", "nada"]), Err(USAGE));
        assert_eq!(parse(&["--snapshot-every", "100"]), Err(USAGE));
        assert_eq!(
            parse(&["--wal", "x.wal", "--snapshot-every", "0"]),
            Err(USAGE)
        );
    }
}
