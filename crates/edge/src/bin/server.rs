use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use edge::core_thread::spawn_core;
use edge::sequencer::Sequencer;
use edge::server::serve;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const USAGE: &str = "uso: server [--listen <endereço:porta>] [--wal <caminho>]";

// Só a própria máquina por padrão: um simulador sem autenticação não deve aparecer na rede
// por acidente (seção 6). Expor é pedir explicitamente com --listen.
const DEFAULT_LISTEN: &str = "127.0.0.1:7000";

const QUEUE: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 não é zero");

#[derive(Debug, PartialEq, Eq)]
struct Args {
    listen: SocketAddr,
    wal: Option<PathBuf>,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut listen = None;
    let mut wal = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let slot_was_empty = if flag == "--listen" {
            let address = value.to_str().and_then(|text| text.parse().ok());
            listen.replace(address.ok_or(USAGE)?).is_none()
        } else if flag == "--wal" {
            wal.replace(PathBuf::from(value)).is_none()
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
    Ok(Args { listen, wal })
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
    let sequencer = match &args.wal {
        None => Sequencer::in_memory(),
        Some(path) => {
            let (sequencer, report) = Sequencer::open(path).map_err(io::Error::other)?;
            eprintln!(
                "log: {} comandos reaplicados, {} bytes descartados",
                report.replayed, report.discarded
            );
            sequencer
        }
    };
    let (core, core_thread) = spawn_core(sequencer, QUEUE)?;

    // O join da thread do núcleo bloqueia; ele mora numa thread comum e avisa por um
    // oneshot, para o runtime nunca ficar preso esperando.
    let (stopped, core_stopped) = oneshot::channel();
    thread::spawn(move || {
        let _ = stopped.send(core_thread.join());
    });

    let listener = TcpListener::bind(args.listen).await?;
    eprintln!("ouvindo em {}", listener.local_addr()?);

    tokio::select! {
        served = serve(listener, core) => served,
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
                wal: None
            })
        );
    }

    #[test]
    fn flags_in_any_order() {
        let expected = Ok(Args {
            listen: "0.0.0.0:9000".parse().unwrap(),
            wal: Some(PathBuf::from("x.wal")),
        });
        assert_eq!(
            parse(&["--listen", "0.0.0.0:9000", "--wal", "x.wal"]),
            expected
        );
        assert_eq!(
            parse(&["--wal", "x.wal", "--listen", "0.0.0.0:9000"]),
            expected
        );
    }

    #[test]
    fn bad_arguments_show_usage() {
        assert_eq!(parse(&["--listen"]), Err(USAGE));
        assert_eq!(parse(&["--listen", "localhost"]), Err(USAGE));
        assert_eq!(parse(&["--port", "7000"]), Err(USAGE));
        assert_eq!(parse(&["--wal", "a", "--wal", "b"]), Err(USAGE));
    }
}
