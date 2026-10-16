mod parse;

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::process::ExitCode;

use edge::market::{ASSETS, asset_id, asset_name, instrument_id, symbol};
use edge::protocol::reject_message;
use edge::sequencer::{Intent, Sequencer, SnapshotBoot};
use exchange_core::domain::AccountId;
use exchange_core::engine::Event;

use crate::parse::{Input, parse_line};

const ACCOUNT: AccountId = AccountId::new(1);

const USAGE: &str = "uso: edge [--wal <caminho> [--snapshot-every <comandos>]]";

fn main() -> ExitCode {
    let result = parse_args(std::env::args_os().skip(1))
        .map_err(io::Error::other)
        .and_then(|log| run(io::stdin().lock(), &mut io::stdout().lock(), log.as_ref()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("erro: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Log {
    wal: PathBuf,
    snapshot_every: Option<NonZeroU64>,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Option<Log>, &'static str> {
    let mut wal = None;
    let mut snapshot_every = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let slot_was_empty = if flag == "--wal" {
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
    match (wal, snapshot_every) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(USAGE),
        (Some(wal), snapshot_every) => Ok(Some(Log {
            wal,
            snapshot_every,
        })),
    }
}

/// Sem WAL, começa do zero em memória. Com WAL, sobe do snapshot e do log antes de aceitar
/// qualquer comando novo.
fn boot(log: Option<&Log>, output: &mut impl Write) -> io::Result<Sequencer> {
    let Some(log) = log else {
        return Ok(Sequencer::in_memory());
    };
    let (sequencer, report) =
        Sequencer::open(&log.wal, log.snapshot_every).map_err(io::Error::other)?;
    writeln!(
        output,
        "log: {} comandos reaplicados, {} bytes descartados",
        report.replayed, report.discarded
    )?;
    if !matches!(report.snapshot, SnapshotBoot::Missing) {
        writeln!(output, "snapshot: {}", report.snapshot)?;
    }
    Ok(sequencer)
}

fn run(input: impl BufRead, output: &mut impl Write, log: Option<&Log>) -> io::Result<()> {
    let mut sequencer = boot(log, output)?;

    for line in input.lines() {
        let line = line?;
        let intent = match parse_line(&line) {
            Ok(Input::Quit) => break,
            Ok(Input::Submit {
                side,
                symbol,
                kind,
                lots,
                time_in_force,
            }) => {
                let Some(instrument) = instrument_id(&symbol) else {
                    writeln!(output, "erro: instrumento desconhecido \"{symbol}\"")?;
                    continue;
                };
                Intent::Order {
                    account: ACCOUNT,
                    instrument,
                    side,
                    kind,
                    lots,
                    time_in_force,
                }
            }
            Ok(Input::Deposit {
                asset: name,
                amount,
            }) => {
                let Some(asset) = asset_id(&name) else {
                    writeln!(output, "erro: ativo desconhecido \"{name}\"")?;
                    continue;
                };
                Intent::Deposit {
                    account: ACCOUNT,
                    asset,
                    amount,
                }
            }
            Ok(Input::Balance) => {
                for (name, asset) in ASSETS {
                    let balance = sequencer.balance(ACCOUNT, asset);
                    writeln!(
                        output,
                        "{name}: disponível {}, reservado {}",
                        balance.available.get(),
                        balance.reserved.get()
                    )?;
                }
                continue;
            }
            Ok(Input::Cancel { symbol, order }) => {
                let Some(instrument) = instrument_id(&symbol) else {
                    writeln!(output, "erro: instrumento desconhecido \"{symbol}\"")?;
                    continue;
                };
                Intent::Cancel {
                    account: ACCOUNT,
                    instrument,
                    order,
                }
            }
            Err(parse::ParseError::Empty) => continue,
            Err(error) => {
                writeln!(output, "erro: {error}")?;
                continue;
            }
        };

        for event in sequencer.submit(intent).map_err(io::Error::other)? {
            writeln!(output, "{}", render(&event))?;
        }
    }
    Ok(())
}

fn render(event: &Event) -> String {
    match event {
        Event::Deposited { asset, amount, .. } => {
            format!("depósito: {} {}", amount.get(), asset_name(*asset))
        }
        Event::DepositRejected { asset, amount, .. } => format!(
            "depósito recusado: {} {} estouraria a oferta total",
            amount.get(),
            asset_name(*asset)
        ),
        Event::Fill {
            instrument,
            fill,
            fee,
            ..
        } => format!(
            "fill {}: taker #{}, maker #{}, {} lots @ {}, taxa {}",
            symbol(*instrument),
            fill.taker.get(),
            fill.maker.get(),
            fill.lots.get(),
            fill.price.get(),
            fee.get()
        ),
        Event::Rested {
            instrument,
            order,
            lots,
            ..
        } => format!(
            "#{} descansou: {} lots em {}",
            order.get(),
            lots.get(),
            symbol(*instrument)
        ),
        Event::Cancelled {
            instrument,
            order,
            lots,
            ..
        } => format!(
            "#{} cancelada: {} lots em {}",
            order.get(),
            lots.get(),
            symbol(*instrument)
        ),
        Event::Expired {
            instrument,
            order,
            lots,
            ..
        } => format!(
            "#{} expirou: {} lots em {}",
            order.get(),
            lots.get(),
            symbol(*instrument)
        ),
        Event::Rejected { order, reason, .. } => {
            format!("#{} rejeitada: {}", order.get(), reject_message(reason))
        }
        Event::Armed {
            instrument,
            order,
            trigger,
            lots,
            ..
        } => format!(
            "#{} armada: {} lots em {}, gatilho {}",
            order.get(),
            lots.get(),
            symbol(*instrument),
            trigger.get()
        ),
        Event::Triggered {
            instrument, order, ..
        } => {
            format!("#{} disparou em {}", order.get(), symbol(*instrument))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FUNDING: &str = "deposit BRL 100000000\ndeposit TESTE 100000\n";

    fn raw_session(input: &str) -> String {
        let mut output = Vec::new();
        run(input.as_bytes(), &mut output, None).expect("I/O em memória não falha");
        String::from_utf8(output).expect("saída é UTF-8")
    }

    // Sessão com a conta já abastecida; as duas linhas de confirmação do depósito saem
    // da saída comparada para os cenários focarem no que testam.
    fn session(input: &str) -> String {
        raw_session(&format!("{FUNDING}{input}"))
            .split_inclusive('\n')
            .skip(2)
            .collect()
    }

    #[test]
    fn deposit_then_balance_shows_reserved_part() {
        let output = raw_session("deposit BRL 50000\nbuy TESTE/BRL 10000 2\nbalance\n");

        assert_eq!(
            output,
            "depósito: 50000 BRL\n\
             #1 descansou: 2 lots em TESTE/BRL\n\
             TESTE: disponível 0, reservado 0\n\
             BRL: disponível 29980, reservado 20020\n\
             OUTRO: disponível 0, reservado 0\n"
        );
    }

    #[test]
    fn order_without_funds_is_rejected() {
        let output = raw_session("buy TESTE/BRL 10000 1\ndeposit OURO 1\n");

        assert_eq!(
            output,
            "#1 rejeitada: saldo insuficiente em BRL: disponível 0, necessário 10010\n\
             erro: ativo desconhecido \"OURO\"\n"
        );
    }

    #[test]
    fn crossing_orders_print_fill_and_remainder() {
        let output = session("sell TESTE/BRL 10000 5\nbuy TESTE/BRL 10100 8\n");

        assert_eq!(
            output,
            "#1 descansou: 5 lots em TESTE/BRL\n\
             fill TESTE/BRL: taker #2, maker #1, 5 lots @ 10000, taxa 50\n\
             #2 descansou: 3 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn invalid_input_reports_error_and_does_not_consume_an_order_id() {
        let output = session("buy TESTE/BRL abc 5\nbuy BTC/BRL 100 1\nsell TESTE/BRL 10000 1\n");

        assert_eq!(
            output,
            "erro: número inválido \"abc\"\n\
             erro: instrumento desconhecido \"BTC/BRL\"\n\
             #1 descansou: 1 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn zero_lots_is_rejected_by_the_core_and_consumes_an_order_id() {
        let output = session("buy TESTE/BRL 100 0\nsell TESTE/BRL 10000 1\n");

        assert_eq!(
            output,
            "#1 rejeitada: ordem precisa de pelo menos 1 lot\n\
             #2 descansou: 1 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn cancel_removes_resting_remainder() {
        let output = session(
            "sell TESTE/BRL 10000 5\nbuy TESTE/BRL 10000 2\ncancel TESTE/BRL 1\ncancel TESTE/BRL 1\n",
        );

        assert_eq!(
            output,
            "#1 descansou: 5 lots em TESTE/BRL\n\
             fill TESTE/BRL: taker #2, maker #1, 2 lots @ 10000, taxa 20\n\
             #1 cancelada: 3 lots em TESTE/BRL\n\
             #1 rejeitada: ordem não está no livro\n"
        );
    }

    #[test]
    fn ioc_cancels_remainder_and_fok_expires_without_fills() {
        let output = session(
            "sell TESTE/BRL 10000 2\nbuy TESTE/BRL 10000 5 ioc\nsell TESTE/BRL 10000 2\nbuy TESTE/BRL 10000 5 fok\n",
        );

        assert_eq!(
            output,
            "#1 descansou: 2 lots em TESTE/BRL\n\
             fill TESTE/BRL: taker #2, maker #1, 2 lots @ 10000, taxa 20\n\
             #2 cancelada: 3 lots em TESTE/BRL\n\
             #3 descansou: 2 lots em TESTE/BRL\n\
             #4 expirou: 5 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn market_sell_sweeps_and_never_rests_while_market_buy_needs_protection() {
        let output = session(
            "buy TESTE/BRL 10500 2\nbuy TESTE/BRL 10000 2\nsell TESTE/BRL mkt 5\nbuy TESTE/BRL mkt 1\n",
        );

        assert_eq!(
            output,
            "#1 descansou: 2 lots em TESTE/BRL\n\
             #2 descansou: 2 lots em TESTE/BRL\n\
             fill TESTE/BRL: taker #3, maker #1, 2 lots @ 10500, taxa 21\n\
             fill TESTE/BRL: taker #3, maker #2, 2 lots @ 10000, taxa 20\n\
             #3 cancelada: 1 lots em TESTE/BRL\n\
             #4 rejeitada: ordem a mercado sem contraparte para servir de referência\n"
        );
    }

    #[test]
    fn off_tick_price_is_rejected_by_the_core_and_consumes_an_order_id() {
        let output = session("buy TESTE/BRL 10005 1\nbuy TESTE/BRL 10010 1\n");

        assert_eq!(
            output,
            "#1 rejeitada: preço 10005 fora do tick 10\n\
             #2 descansou: 1 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn protected_market_buy_sweeps_only_within_protection() {
        let output = session(
            "sell TESTE/BRL 10000 2\nsell TESTE/BRL 10600 2\nbuy TESTE/BRL mkt 5 max 10500\n",
        );

        assert_eq!(
            output,
            "#1 descansou: 2 lots em TESTE/BRL\n\
             #2 descansou: 2 lots em TESTE/BRL\n\
             fill TESTE/BRL: taker #3, maker #1, 2 lots @ 10000, taxa 20\n\
             #3 cancelada: 3 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn fat_finger_and_oversized_orders_are_rejected_and_plain_market_buy_works() {
        let output = session(
            "sell TESTE/BRL 10000 2\nbuy TESTE/BRL 100000 1\nbuy TESTE/BRL 10000 1001\nbuy TESTE/BRL mkt 1\n",
        );

        assert_eq!(
            output,
            "#1 descansou: 2 lots em TESTE/BRL\n\
             #2 rejeitada: preço 100000 fora do colar (limite 11000)\n\
             #3 rejeitada: 1001 lots passa do máximo de 1000\n\
             fill TESTE/BRL: taker #4, maker #1, 1 lots @ 10000, taxa 10\n"
        );
    }

    #[test]
    fn blank_lines_are_ignored() {
        let output = session("\n   \nsell TESTE/BRL 10000 1\n");

        assert_eq!(output, "#1 descansou: 1 lots em TESTE/BRL\n");
    }

    #[test]
    fn quit_stops_reading() {
        let output = session("quit\nsell TESTE/BRL 10000 1\n");

        assert_eq!(output, "");
    }

    mod wal {
        use std::fs::{self, OpenOptions};
        use std::path::Path;

        use exchange_wal::Wal;

        use edge::sequencer::snapshot_path;

        use super::*;

        /// Caminho temporário que se apaga no fim do teste, inclusive se ele falhar.
        struct TempPath(PathBuf);

        impl TempPath {
            fn new(name: &str) -> Self {
                let file = format!("exchange-edge-{}-{name}", std::process::id());
                let path = std::env::temp_dir().join(file);
                remove_if_present(&path);
                Self(path)
            }
        }

        impl Drop for TempPath {
            fn drop(&mut self) {
                remove_if_present(&self.0);
                remove_if_present(&snapshot_path(&self.0));
            }
        }

        fn remove_if_present(path: &Path) {
            match fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    panic!("não deu para limpar {}: {error}", path.display())
                }
                _ => {}
            }
        }

        fn wal_session(path: &TempPath, input: &str) -> io::Result<String> {
            logged_session(path, None, input)
        }

        fn logged_session(
            path: &TempPath,
            snapshot_every: Option<u64>,
            input: &str,
        ) -> io::Result<String> {
            let log = Log {
                wal: path.0.clone(),
                snapshot_every: snapshot_every.map(|every| NonZeroU64::new(every).unwrap()),
            };
            let mut output = Vec::new();
            run(input.as_bytes(), &mut output, Some(&log))?;
            Ok(String::from_utf8(output).expect("saída é UTF-8"))
        }

        // Com snapshot a cada 2 comandos, o reinício carrega o estado dos 4 primeiros e só
        // reaplica o quinto; o que se vê depois é o mesmo de quem nunca reiniciou.
        #[test]
        fn restart_from_a_snapshot_replays_only_the_rest_of_the_log() {
            let path = TempPath::new("snapshot");
            let history = "sell TESTE/BRL 10000 5\nbuy TESTE/BRL 10100 2\nbuy TESTE/BRL 9000 1\n";
            let uninterrupted = raw_session(&format!("{FUNDING}{history}balance\n"));

            logged_session(&path, Some(2), &format!("{FUNDING}{history}")).unwrap();
            let restarted = wal_session(&path, "balance\n").unwrap();

            assert_eq!(
                restarted.lines().take(2).collect::<Vec<_>>(),
                [
                    "log: 1 comandos reaplicados, 0 bytes descartados",
                    "snapshot: carregado, cobre 4 comandos"
                ]
            );
            assert_eq!(last_lines(&restarted, 2), last_lines(&uninterrupted, 2));
        }

        fn last_lines(output: &str, count: usize) -> Vec<&str> {
            let lines: Vec<&str> = output.lines().collect();
            lines[lines.len() - count..].to_vec()
        }

        #[test]
        fn fresh_log_boots_empty() {
            let path = TempPath::new("fresh");

            let output = wal_session(&path, "").unwrap();

            assert_eq!(output, "log: 0 comandos reaplicados, 0 bytes descartados\n");
        }

        // O critério da F3 em miniatura: o estado depois de reiniciar é o mesmo de uma
        // sessão que nunca caiu.
        #[test]
        fn restart_rebuilds_the_same_state_as_an_uninterrupted_session() {
            let path = TempPath::new("restart");
            let history = "sell TESTE/BRL 10000 5\nbuy TESTE/BRL 10100 2\nbuy TESTE/BRL 9000 1\n";
            let uninterrupted = raw_session(&format!("{FUNDING}{history}balance\n"));

            wal_session(&path, &format!("{FUNDING}{history}")).unwrap();
            let restarted = wal_session(&path, "balance\n").unwrap();

            assert_eq!(
                restarted.lines().next(),
                Some("log: 5 comandos reaplicados, 0 bytes descartados")
            );
            assert_eq!(last_lines(&restarted, 2), last_lines(&uninterrupted, 2));
        }

        #[test]
        fn resting_orders_survive_restart_and_still_match() {
            let path = TempPath::new("book");
            wal_session(&path, &format!("{FUNDING}sell TESTE/BRL 10000 5\n")).unwrap();

            let output = wal_session(&path, "buy TESTE/BRL 10000 2\n").unwrap();

            assert_eq!(
                last_lines(&output, 1),
                ["fill TESTE/BRL: taker #2, maker #1, 2 lots @ 10000, taxa 20"]
            );
        }

        #[test]
        fn order_ids_continue_after_restart() {
            let path = TempPath::new("ids");
            wal_session(&path, &format!("{FUNDING}sell TESTE/BRL 10000 1\n")).unwrap();
            wal_session(&path, "cancel TESTE/BRL 1\n").unwrap();

            let output = wal_session(&path, "sell TESTE/BRL 10000 1\n").unwrap();

            assert_eq!(
                last_lines(&output, 1),
                ["#2 descansou: 1 lots em TESTE/BRL"]
            );
        }

        #[test]
        fn torn_tail_is_cut_and_reported_on_boot() {
            let path = TempPath::new("torn");
            wal_session(&path, FUNDING).unwrap();
            let mut raw = OpenOptions::new().append(true).open(&path.0).unwrap();
            raw.write_all(&[50, 0, 0]).unwrap();

            let output = wal_session(&path, "").unwrap();

            assert_eq!(output, "log: 2 comandos reaplicados, 3 bytes descartados\n");
        }

        #[test]
        fn intact_frame_that_is_not_a_record_fails_the_boot() {
            let path = TempPath::new("invalid");
            Wal::open(&path.0).unwrap().wal.append(b"lixo").unwrap();

            let error = wal_session(&path, "").unwrap_err();

            assert!(error.to_string().starts_with("log inválido:"), "{error}");
        }

        #[test]
        fn args_select_the_wal_path_and_the_snapshot_interval() {
            let parse = |list: &[&str]| parse_args(list.iter().map(OsString::from));
            let log = |snapshot_every: Option<u64>| {
                Ok(Some(Log {
                    wal: PathBuf::from("x.wal"),
                    snapshot_every: snapshot_every.and_then(NonZeroU64::new),
                }))
            };

            assert_eq!(parse(&[]), Ok(None));
            assert_eq!(parse(&["--wal", "x.wal"]), log(None));
            assert_eq!(
                parse(&["--wal", "x.wal", "--snapshot-every", "50"]),
                log(Some(50))
            );
            assert_eq!(
                parse(&["--snapshot-every", "50", "--wal", "x.wal"]),
                log(Some(50))
            );
            assert_eq!(parse(&["--wal"]), Err(USAGE));
            assert_eq!(parse(&["-w", "x.wal"]), Err(USAGE));
            assert_eq!(parse(&["--wal", "x.wal", "extra"]), Err(USAGE));
            assert_eq!(parse(&["--snapshot-every", "50"]), Err(USAGE));
            assert_eq!(
                parse(&["--wal", "x.wal", "--snapshot-every", "0"]),
                Err(USAGE)
            );
            assert_eq!(parse(&["--wal", "a", "--wal", "b"]), Err(USAGE));
        }
    }
}
