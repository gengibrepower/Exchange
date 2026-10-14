mod parse;

use std::io::{self, BufRead, Write};

use exchange_core::domain::{
    AccountId, Amount, AssetId, Bps, Instrument, InstrumentId, Lots, OrderId, OrderRequest, Price,
    Seq,
};
use exchange_core::engine::{
    Command, Engine, Event, FeeSchedule, ListingConfig, RejectReason, RiskLimits,
};

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

fn main() -> io::Result<()> {
    run(io::stdin().lock(), &mut io::stdout().lock())
}

fn run(input: impl BufRead, output: &mut impl Write) -> io::Result<()> {
    // Taxa de taker de 10 bps (0,1%) para a casa: pequena, mas exercita o caminho (decisão 7).
    let fees = FeeSchedule {
        house: HOUSE,
        taker_bps: Bps::new(10).expect("10 bps é menos que 100%"),
    };
    let mut engine = Engine::new(listings(), fees).expect("instrumentos fixos não se repetem");
    let mut next_seq: u64 = 1;

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
                    id: OrderId::new(next_seq),
                    account: ACCOUNT,
                    instrument,
                    side,
                    kind,
                    total: lots,
                    seq: Seq::new(next_seq),
                    time_in_force,
                };
                next_seq += 1;
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
        run(input.as_bytes(), &mut output).expect("I/O em memória não falha");
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
    fn invalid_input_reports_error_and_does_not_consume_seq() {
        let output = session("buy TESTE/BRL abc 5\nbuy BTC/BRL 100 1\nsell TESTE/BRL 10000 1\n");

        assert_eq!(
            output,
            "erro: número inválido \"abc\"\n\
             erro: instrumento desconhecido \"BTC/BRL\"\n\
             #1 descansou: 1 lots em TESTE/BRL\n"
        );
    }

    #[test]
    fn zero_lots_is_rejected_by_the_core_and_consumes_seq() {
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
    fn off_tick_price_is_rejected_by_the_core_and_consumes_seq() {
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
}
