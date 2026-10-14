mod parse;

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use exchange_core::domain::{
    AccountId, Amount, AssetId, Bps, Instrument, InstrumentId, Lots, OrderId, OrderRequest, Price,
    Seq,
};
use exchange_core::engine::{
    Command, Engine, Event, FeeSchedule, ListingConfig, RejectReason, RiskLimits,
};
use exchange_core::record::{LogRecord, RecordError};
use exchange_wal::Wal;

use crate::parse::{Input, parse_line};

const INSTRUMENTS: [(&str, InstrumentId); 1] = [("TESTE/BRL", InstrumentId::new(1))];
const ACCOUNT: AccountId = AccountId::new(1);
const HOUSE: AccountId = AccountId::new(0);
const TESTE: AssetId = AssetId::new(1);
const BRL: AssetId = AssetId::new(2);
const ASSETS: [(&str, AssetId); 2] = [("TESTE", TESTE), ("BRL", BRL)];

// TESTE/BRL: preço em centavos por lot, tick de 10 centavos, 1 lot = 100 unidades de TESTE.
// Risco: no máximo 1.000 lots por ordem e colar de 10% sobre o melhor preço oposto.
fn listings() -> [ListingConfig; 1] {
    let tick = Price::new(10).expect("tick fixo é positivo");
    let lot_size = Amount::new(100).expect("lot fixo é positivo");
    let instrument = Instrument::new(INSTRUMENTS[0].1, TESTE, BRL, tick, lot_size)
        .expect("especificação fixa é válida");
    let risk = RiskLimits {
        max_lots: Lots::new(1_000).expect("limite fixo é válido"),
        collar: Bps::new(1_000).expect("10% é menos que 100%"),
    };
    [ListingConfig { instrument, risk }]
}

const USAGE: &str = "uso: edge [--wal <caminho>]";

fn main() -> ExitCode {
    let result = parse_args(std::env::args_os().skip(1))
        .map_err(io::Error::other)
        .and_then(|wal| run(io::stdin().lock(), &mut io::stdout().lock(), wal.as_deref()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("erro: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Option<PathBuf>, &'static str> {
    match (args.next(), args.next(), args.next()) {
        (None, _, _) => Ok(None),
        (Some(flag), Some(path), None) if flag == "--wal" => Ok(Some(PathBuf::from(path))),
        _ => Err(USAGE),
    }
}

/// Próximos números a atribuir. O `seq` avança a cada comando gravado e é a posição no
/// log, que também dá a prioridade FIFO (decisão 8); o id de ordem só avança em ordens e
/// é o `#n` que o usuário vê.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Counters {
    next_seq: u64,
    next_order_id: u64,
}

impl Counters {
    const FRESH: Self = Self {
        next_seq: 1,
        next_order_id: 1,
    };
}

/// Reaplica o log no engine, sem imprimir: esses eventos já foram respondidos na sessão
/// que os gravou. Os contadores saem do próprio log, nada é guardado à parte.
fn replay(engine: &mut Engine, records: &[Vec<u8>]) -> Result<Counters, RecordError> {
    let mut counters = Counters::FRESH;
    for bytes in records {
        let record = LogRecord::from_bytes(bytes)?;
        let command = record.command();
        if let Command::Submit(request) = command {
            let after = request.id.get().saturating_add(1);
            counters.next_order_id = counters.next_order_id.max(after);
        }
        counters.next_seq = record.seq().get().saturating_add(1);
        engine.apply(command);
    }
    Ok(counters)
}

/// Sem WAL, começa do zero em memória. Com WAL, abre (cortando rabo rasgado) e reaplica
/// o log antes de aceitar qualquer comando novo.
fn boot(
    engine: &mut Engine,
    wal_path: Option<&Path>,
    output: &mut impl Write,
) -> io::Result<(Option<Wal>, Counters)> {
    let Some(path) = wal_path else {
        return Ok((None, Counters::FRESH));
    };
    let recovery = Wal::open(path).map_err(io::Error::other)?;
    // Rabo rasgado o open já cortou; um quadro íntegro que não decodifica é bug ou
    // formato incompatível, e seguir sem ele quebraria o replay. Fail-stop.
    let counters = replay(engine, &recovery.records)
        .map_err(|error| io::Error::other(format!("log inválido: {error}")))?;
    writeln!(
        output,
        "log: {} comandos reaplicados, {} bytes descartados",
        recovery.records.len(),
        recovery.discarded
    )?;
    Ok((Some(recovery.wal), counters))
}

fn run(input: impl BufRead, output: &mut impl Write, wal_path: Option<&Path>) -> io::Result<()> {
    // Taxa de taker de 10 bps (0,1%) para a casa: pequena, mas exercita o caminho (decisão 7).
    let fees = FeeSchedule {
        house: HOUSE,
        taker_bps: Bps::new(10).expect("10 bps é menos que 100%"),
    };
    let mut engine = Engine::new(listings(), fees).expect("instrumentos fixos não se repetem");
    let (mut wal, mut counters) = boot(&mut engine, wal_path, output)?;

    for line in input.lines() {
        let line = line?;
        let command = match parse_line(&line) {
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
                let request = OrderRequest {
                    id: OrderId::new(counters.next_order_id),
                    account: ACCOUNT,
                    instrument,
                    side,
                    kind,
                    total: lots,
                    seq: Seq::new(counters.next_seq),
                    time_in_force,
                };
                counters.next_order_id += 1;
                Command::Submit(request)
            }
            Ok(Input::Deposit {
                asset: name,
                amount,
            }) => {
                let Some(asset) = asset_id(&name) else {
                    writeln!(output, "erro: ativo desconhecido \"{name}\"")?;
                    continue;
                };
                Command::Deposit {
                    account: ACCOUNT,
                    asset,
                    amount,
                }
            }
            Ok(Input::Balance) => {
                for (name, asset) in ASSETS {
                    let balance = engine.balance(ACCOUNT, asset);
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
                Command::Cancel {
                    instrument,
                    account: ACCOUNT,
                    order,
                }
            }
            Err(parse::ParseError::Empty) => continue,
            Err(error) => {
                writeln!(output, "erro: {error}")?;
                continue;
            }
        };

        let seq = Seq::new(counters.next_seq);
        counters.next_seq += 1;
        if let Some(wal) = wal.as_mut() {
            // Durável antes de aplicar e de responder (seção 4.6). Falhou, para: o comando
            // não é aplicado e o WAL fica envenenado.
            let record = LogRecord::new(seq, command);
            wal.append(&record.to_bytes()).map_err(io::Error::other)?;
        }

        for event in engine.apply(command) {
            writeln!(output, "{}", render(&event))?;
        }
    }
    Ok(())
}

fn asset_id(name: &str) -> Option<AssetId> {
    ASSETS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, asset)| *asset)
}

fn asset_name(asset: AssetId) -> &'static str {
    ASSETS
        .iter()
        .find(|(_, id)| *id == asset)
        .map_or("?", |(name, _)| name)
}

fn instrument_id(symbol: &str) -> Option<InstrumentId> {
    INSTRUMENTS
        .iter()
        .find(|(name, _)| *name == symbol)
        .map(|(_, instrument)| *instrument)
}

fn symbol(instrument: InstrumentId) -> &'static str {
    INSTRUMENTS
        .iter()
        .find(|(_, id)| *id == instrument)
        .map_or("?", |(name, _)| name)
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
        } => format!(
            "#{} expirou: {} lots em {}",
            order.get(),
            lots.get(),
            symbol(*instrument)
        ),
        Event::Rejected { order, reason } => {
            let reason = match reason {
                RejectReason::UnknownInstrument(_) => "instrumento desconhecido".to_string(),
                RejectReason::EmptyOrder => "ordem precisa de pelo menos 1 lot".to_string(),
                RejectReason::OffTick { price, tick } => {
                    format!("preço {} fora do tick {}", price.get(), tick.get())
                }
                RejectReason::OrderTooLarge { lots, max } => {
                    format!("{} lots passa do máximo de {}", lots.get(), max.get())
                }
                RejectReason::OutsideCollar { bound, edge } => format!(
                    "preço {} fora do colar (limite {})",
                    bound.get(),
                    edge.get()
                ),
                RejectReason::NoReferencePrice => {
                    "ordem a mercado sem contraparte para servir de referência".to_string()
                }
                RejectReason::ValueOverflow => "valor da ordem estoura o limite".to_string(),
                RejectReason::InsufficientFunds {
                    asset,
                    available,
                    required,
                } => format!(
                    "saldo insuficiente em {}: disponível {}, necessário {}",
                    asset_name(*asset),
                    available.get(),
                    required.get()
                ),
                RejectReason::DuplicateOrderId => "id de ordem já em uso".to_string(),
                RejectReason::HouseCannotTrade => "a conta da casa não negocia".to_string(),
                RejectReason::UnknownOrder => "ordem não está no livro".to_string(),
                RejectReason::NotOwner => "ordem pertence a outra conta".to_string(),
            };
            format!("#{} rejeitada: {reason}", order.get())
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
             BRL: disponível 29980, reservado 20020\n"
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
            let mut output = Vec::new();
            run(input.as_bytes(), &mut output, Some(&path.0))?;
            Ok(String::from_utf8(output).expect("saída é UTF-8"))
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
        fn args_select_the_wal_path() {
            let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();

            assert_eq!(parse_args(args(&[]).into_iter()), Ok(None));
            assert_eq!(
                parse_args(args(&["--wal", "x.wal"]).into_iter()),
                Ok(Some(PathBuf::from("x.wal")))
            );
            assert_eq!(parse_args(args(&["--wal"]).into_iter()), Err(USAGE));
            assert_eq!(parse_args(args(&["-w", "x.wal"]).into_iter()), Err(USAGE));
            assert_eq!(
                parse_args(args(&["--wal", "x.wal", "extra"]).into_iter()),
                Err(USAGE)
            );
        }
    }
}
