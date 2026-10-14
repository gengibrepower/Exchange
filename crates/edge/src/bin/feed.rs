use std::ffi::OsString;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::process::ExitCode;

use edge::feed_client::{Action, follow, render};
use edge::market_data::Feed;
use tokio::net::TcpStream;

const USAGE: &str = "uso: feed [--connect <endereço:porta>] [--instrument <símbolo>] \
                     [--feed l1|l2] [--plain]";
const DEFAULT_CONNECT: &str = "127.0.0.1:7000";
const DEFAULT_INSTRUMENT: &str = "TESTE/BRL";
const DEPTH: usize = 10;

// Volta o cursor para o canto e apaga a tela: redesenho sem dependência.
const CLEAR: &str = "\x1b[2J\x1b[H";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    connect: SocketAddr,
    instrument: String,
    feed: Feed,
    plain: bool,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut connect = None;
    let mut instrument = None;
    let mut feed = None;
    let mut plain = false;
    while let Some(flag) = args.next() {
        if flag == "--plain" {
            plain = true;
            continue;
        }
        let value = args.next().ok_or(USAGE)?;
        let value = value.to_str().ok_or(USAGE)?;
        let was_empty = if flag == "--connect" {
            connect.replace(value.parse().map_err(|_| USAGE)?).is_none()
        } else if flag == "--instrument" {
            instrument.replace(value.to_string()).is_none()
        } else if flag == "--feed" {
            let parsed = match value {
                "l1" => Feed::L1,
                "l2" => Feed::L2,
                _ => return Err(USAGE),
            };
            feed.replace(parsed).is_none()
        } else {
            return Err(USAGE);
        };
        if !was_empty {
            return Err(USAGE);
        }
    }
    let connect = match connect {
        Some(address) => address,
        None => DEFAULT_CONNECT.parse().map_err(|_| USAGE)?,
    };
    Ok(Args {
        connect,
        instrument: instrument.unwrap_or_else(|| DEFAULT_INSTRUMENT.to_string()),
        feed: feed.unwrap_or(Feed::L2),
        plain,
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
    let stream = TcpStream::connect(args.connect).await?;
    let mut out = io::stdout().lock();
    let mut written = Ok(());
    follow(stream, &args.instrument, args.feed, |tracker, action| {
        written = show(&mut out, args.plain, tracker.book(), action);
        if written.is_err() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .await?;
    written?;
    eprintln!("o servidor fechou a conexão");
    Ok(())
}

fn show(
    out: &mut impl Write,
    plain: bool,
    book: Option<&edge::market_data::LocalBook>,
    action: &Action,
) -> io::Result<()> {
    match action {
        Action::Redraw => {
            let Some(book) = book else { return Ok(()) };
            if !plain {
                out.write_all(CLEAR.as_bytes())?;
            }
            out.write_all(render(book, DEPTH).as_bytes())?;
            if plain {
                writeln!(out)?;
            }
            out.flush()
        }
        Action::Resubscribe(gap) => writeln!(
            out,
            "buraco na sequência: esperava {}, veio {} — pedindo snapshot novo",
            gap.expected, gap.found
        ),
        Action::ServerError(message) => writeln!(out, "erro do servidor: {message}"),
        Action::Ignore => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Args, &'static str> {
        parse_args(list.iter().map(OsString::from))
    }

    #[test]
    fn defaults_follow_l2_of_the_only_instrument_on_loopback() {
        assert_eq!(
            parse(&[]),
            Ok(Args {
                connect: "127.0.0.1:7000".parse().unwrap(),
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2,
                plain: false,
            })
        );
    }

    #[test]
    fn every_flag_in_any_order() {
        assert_eq!(
            parse(&[
                "--plain",
                "--feed",
                "l1",
                "--connect",
                "10.0.0.1:9000",
                "--instrument",
                "X/Y"
            ]),
            Ok(Args {
                connect: "10.0.0.1:9000".parse().unwrap(),
                instrument: "X/Y".to_string(),
                feed: Feed::L1,
                plain: true,
            })
        );
    }

    #[test]
    fn bad_arguments_show_usage() {
        assert_eq!(parse(&["--feed", "l3"]), Err(USAGE));
        assert_eq!(parse(&["--connect"]), Err(USAGE));
        assert_eq!(parse(&["--connect", "localhost"]), Err(USAGE));
        assert_eq!(parse(&["--feed", "l1", "--feed", "l2"]), Err(USAGE));
        assert_eq!(parse(&["--depth", "3"]), Err(USAGE));
    }
}
