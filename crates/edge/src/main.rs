mod parse;

use std::io::{self, BufRead, Write};

use exchange_core::domain::{AccountId, InstrumentId, Order, OrderId, Seq};
use exchange_core::engine::{Command, Engine, Event, RejectReason};

use crate::parse::{Input, parse_line};

const INSTRUMENTS: [(&str, InstrumentId); 1] = [("TESTE/BRL", InstrumentId::new(1))];
const ACCOUNT: AccountId = AccountId::new(1);

fn main() -> io::Result<()> {
    run(io::stdin().lock(), &mut io::stdout().lock())
}

fn run(input: impl BufRead, output: &mut impl Write) -> io::Result<()> {
    let mut engine = Engine::new(INSTRUMENTS.map(|(_, instrument)| instrument));
    let mut next_seq: u64 = 1;

    for line in input.lines() {
        let line = line?;
        let command = match parse_line(&line) {
            Ok(Input::Quit) => break,
            Ok(Input::Submit {
                side,
                symbol,
                price,
                lots,
            }) => {
                let Some(instrument) = instrument_id(&symbol) else {
                    writeln!(output, "erro: instrumento desconhecido \"{symbol}\"")?;
                    continue;
                };
                let order = match Order::new(
                    OrderId::new(next_seq),
                    ACCOUNT,
                    instrument,
                    side,
                    price,
                    lots,
                    Seq::new(next_seq),
                ) {
                    Ok(order) => order,
                    Err(error) => {
                        writeln!(output, "erro: {error}")?;
                        continue;
                    }
                };
                next_seq += 1;
                Command::Submit(order)
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

        for event in engine.apply(command) {
            writeln!(output, "{}", render(&event))?;
        }
    }
    Ok(())
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
        Event::Fill { instrument, fill } => format!(
            "fill {}: taker #{}, maker #{}, {} lots @ {}",
            symbol(*instrument),
            fill.taker.get(),
            fill.maker.get(),
            fill.lots.get(),
            fill.price.get()
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
        Event::Rejected { order, reason } => {
            let reason = match reason {
                RejectReason::UnknownInstrument(_) => "instrumento desconhecido".to_string(),
                RejectReason::NotNew(status) => format!("ordem já estava em {status:?}"),
                RejectReason::DuplicateOrderId => "id de ordem já em uso".to_string(),
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

    fn session(input: &str) -> String {
        let mut output = Vec::new();
        run(input.as_bytes(), &mut output).expect("I/O em memória não falha");
        String::from_utf8(output).expect("saída é UTF-8")
    }

    #[test]
    fn crossing_orders_print_fill_and_remainder() {
        let output = session("sell TESTE/BRL 10000 5\nbuy TESTE/BRL 10100 8\n");

        assert_eq!(
            output,
            "#1 descansou: 5 lots em TESTE/BRL\n\
             fill TESTE/BRL: taker #2, maker #1, 5 lots @ 10000\n\
             #2 descansou: 3 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn invalid_input_reports_error_and_does_not_consume_seq() {
        let output = session(
            "buy TESTE/BRL abc 5\nbuy TESTE/BRL 100 0\nbuy BTC/BRL 100 1\nsell TESTE/BRL 10000 1\n",
        );

        assert_eq!(
            output,
            "erro: número inválido \"abc\"\n\
             erro: ordem precisa de pelo menos 1 lot\n\
             erro: instrumento desconhecido \"BTC/BRL\"\n\
             #1 descansou: 1 lots em TESTE/BRL\n"
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
             fill TESTE/BRL: taker #2, maker #1, 2 lots @ 10000\n\
             #1 cancelada: 3 lots em TESTE/BRL\n\
             #1 rejeitada: ordem não está no livro\n"
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
}
