use std::str::FromStr;

use exchange_core::domain::{
    Amount, DomainError, Lots, OrderId, OrderKind, Price, Side, TimeInForce,
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    Submit {
        side: Side,
        symbol: String,
        kind: OrderKind,
        lots: Lots,
        time_in_force: TimeInForce,
    },
    Cancel {
        symbol: String,
        order: OrderId,
    },
    Deposit {
        asset: String,
        amount: Amount,
    },
    Balance,
    Quit,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ParseError {
    Empty,
    UnknownCommand(String),
    WrongArity,
    InvalidNumber(String),
    UnknownTimeInForce(String),
    ProtectionNeedsMarket,
    StopTakesNoProtection,
    Domain(DomainError),
}

/// O que vem depois dos lots, antes do time-in-force: no máximo um dos dois.
#[derive(Clone, Copy)]
enum Modifier<'a> {
    Protection(&'a str),
    Trigger(&'a str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "linha vazia"),
            Self::UnknownCommand(command) => write!(f, "comando desconhecido \"{command}\""),
            Self::WrongArity => write!(
                f,
                "uso: buy|sell <instrumento> <preço|mkt> <lots> [max <proteção> | stop <gatilho>] [gtc|ioc|fok] | cancel <instrumento> <id> | deposit <ativo> <quantia> | balance | quit"
            ),
            Self::InvalidNumber(token) => write!(f, "número inválido \"{token}\""),
            Self::ProtectionNeedsMarket => {
                write!(f, "\"max\" só vale para ordem a mercado (preço mkt)")
            }
            Self::StopTakesNoProtection => {
                write!(f, "stop não leva \"max\": o teto vem do gatilho e do colar")
            }
            Self::UnknownTimeInForce(token) => {
                write!(
                    f,
                    "time-in-force desconhecido \"{token}\" (use gtc, ioc ou fok)"
                )
            }
            Self::Domain(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// # Errors
/// [`ParseError`] descrevendo o primeiro problema encontrado na linha.
pub fn parse_line(line: &str) -> Result<Input, ParseError> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    match tokens.as_slice() {
        [] => Err(ParseError::Empty),
        ["quit"] => Ok(Input::Quit),
        [side @ ("buy" | "sell"), symbol, price, lots, options @ ..] => {
            let side = if *side == "buy" { Side::Bid } else { Side::Ask };
            let (modifier, time_in_force) = match options {
                [] => (None, "gtc"),
                [tif] => (None, *tif),
                ["max", bound] => (Some(Modifier::Protection(bound)), "gtc"),
                ["max", bound, tif] => (Some(Modifier::Protection(bound)), *tif),
                ["stop", trigger] => (Some(Modifier::Trigger(trigger)), "gtc"),
                ["stop", trigger, tif] => (Some(Modifier::Trigger(trigger)), *tif),
                ["max", _, "stop", ..] | ["stop", _, "max", ..] => {
                    return Err(ParseError::StopTakesNoProtection);
                }
                _ => return Err(ParseError::WrongArity),
            };
            submit(side, symbol, price, lots, modifier, time_in_force)
        }
        ["cancel", symbol, id] => Ok(Input::Cancel {
            symbol: symbol.to_string(),
            order: OrderId::new(number(id)?),
        }),
        ["deposit", asset, amount] => Ok(Input::Deposit {
            asset: asset.to_string(),
            amount: Amount::new(number(amount)?).map_err(ParseError::Domain)?,
        }),
        ["balance"] => Ok(Input::Balance),
        [
            "buy" | "sell" | "cancel" | "deposit" | "balance" | "quit",
            ..,
        ] => Err(ParseError::WrongArity),
        [command, ..] => Err(ParseError::UnknownCommand(command.to_string())),
    }
}

fn submit(
    side: Side,
    symbol: &str,
    price: &str,
    lots: &str,
    modifier: Option<Modifier>,
    time_in_force: &str,
) -> Result<Input, ParseError> {
    let kind = match (price, modifier) {
        ("mkt", None) => OrderKind::Market { protection: None },
        ("mkt", Some(Modifier::Protection(bound))) => OrderKind::Market {
            protection: Some(price_of(bound)?),
        },
        (_, Some(Modifier::Protection(_))) => return Err(ParseError::ProtectionNeedsMarket),
        ("mkt", Some(Modifier::Trigger(trigger))) => OrderKind::Stop {
            trigger: price_of(trigger)?,
            limit: None,
        },
        (limit, Some(Modifier::Trigger(trigger))) => OrderKind::Stop {
            trigger: price_of(trigger)?,
            limit: Some(price_of(limit)?),
        },
        (limit, None) => OrderKind::Limit(price_of(limit)?),
    };
    let lots = Lots::new(number(lots)?).map_err(ParseError::Domain)?;
    let time_in_force = match time_in_force {
        "gtc" => TimeInForce::Gtc,
        "ioc" => TimeInForce::Ioc,
        "fok" => TimeInForce::Fok,
        other => return Err(ParseError::UnknownTimeInForce(other.to_string())),
    };
    Ok(Input::Submit {
        side,
        symbol: symbol.to_string(),
        kind,
        lots,
        time_in_force,
    })
}

fn number<T: FromStr>(token: &str) -> Result<T, ParseError> {
    token
        .parse()
        .map_err(|_| ParseError::InvalidNumber(token.to_string()))
}

fn price_of(token: &str) -> Result<Price, ParseError> {
    Price::new(number(token)?).map_err(ParseError::Domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit(side: Side, price: i64, lots: i64) -> Input {
        Input::Submit {
            side,
            symbol: "TESTE/BRL".to_string(),
            kind: OrderKind::Limit(Price::new(price).expect("preço de teste válido")),
            lots: Lots::new(lots).expect("lots de teste válidos"),
            time_in_force: TimeInForce::Gtc,
        }
    }

    #[test]
    fn parses_buy_as_bid() {
        assert_eq!(
            parse_line("buy TESTE/BRL 10000 5"),
            Ok(submit(Side::Bid, 10_000, 5))
        );
    }

    #[test]
    fn parses_sell_as_ask() {
        assert_eq!(
            parse_line("sell TESTE/BRL 10000 5"),
            Ok(submit(Side::Ask, 10_000, 5))
        );
    }

    #[test]
    fn tolerates_extra_whitespace() {
        assert_eq!(
            parse_line("  buy   TESTE/BRL  10000   5 "),
            Ok(submit(Side::Bid, 10_000, 5))
        );
    }

    #[test]
    fn parses_optional_time_in_force() {
        let Ok(Input::Submit { time_in_force, .. }) = parse_line("buy TESTE/BRL 100 5 ioc") else {
            panic!("deveria ser uma submissão");
        };
        assert_eq!(time_in_force, TimeInForce::Ioc);

        let Ok(Input::Submit { time_in_force, .. }) = parse_line("sell TESTE/BRL 100 5 fok") else {
            panic!("deveria ser uma submissão");
        };
        assert_eq!(time_in_force, TimeInForce::Fok);
    }

    #[test]
    fn rejects_unknown_time_in_force() {
        assert_eq!(
            parse_line("buy TESTE/BRL 100 5 day"),
            Err(ParseError::UnknownTimeInForce("day".to_string()))
        );
    }

    #[test]
    fn parses_market_order() {
        let Ok(Input::Submit { kind, .. }) = parse_line("buy TESTE/BRL mkt 5") else {
            panic!("deveria ser uma submissão");
        };
        assert_eq!(kind, OrderKind::Market { protection: None });
    }

    #[test]
    fn parses_market_protection_with_optional_time_in_force() {
        let Ok(Input::Submit {
            kind,
            time_in_force,
            ..
        }) = parse_line("buy TESTE/BRL mkt 5 max 10500 fok")
        else {
            panic!("deveria ser uma submissão");
        };
        assert_eq!(
            kind,
            OrderKind::Market {
                protection: Some(Price::new(10_500).unwrap())
            }
        );
        assert_eq!(time_in_force, TimeInForce::Fok);
    }

    #[test]
    fn rejects_protection_on_limit_order_and_bad_bound() {
        assert_eq!(
            parse_line("buy TESTE/BRL 10000 5 max 10500"),
            Err(ParseError::ProtectionNeedsMarket)
        );
        assert_eq!(
            parse_line("buy TESTE/BRL mkt 5 max zero"),
            Err(ParseError::InvalidNumber("zero".to_string()))
        );
    }

    #[test]
    fn parses_deposit_and_balance() {
        assert_eq!(
            parse_line("deposit BRL 500"),
            Ok(Input::Deposit {
                asset: "BRL".to_string(),
                amount: Amount::new(500).unwrap(),
            })
        );
        assert_eq!(parse_line("balance"), Ok(Input::Balance));
        assert_eq!(
            parse_line("deposit BRL -1"),
            Err(ParseError::Domain(DomainError::NegativeAmount(-1)))
        );
        assert_eq!(parse_line("balance BRL"), Err(ParseError::WrongArity));
    }

    #[test]
    fn parses_quit() {
        assert_eq!(parse_line("quit"), Ok(Input::Quit));
    }

    #[test]
    fn parses_cancel() {
        assert_eq!(
            parse_line("cancel TESTE/BRL 7"),
            Ok(Input::Cancel {
                symbol: "TESTE/BRL".to_string(),
                order: OrderId::new(7),
            })
        );
    }

    #[test]
    fn rejects_cancel_with_bad_id() {
        assert_eq!(
            parse_line("cancel TESTE/BRL -1"),
            Err(ParseError::InvalidNumber("-1".to_string()))
        );
        assert_eq!(parse_line("cancel TESTE/BRL"), Err(ParseError::WrongArity));
    }

    #[test]
    fn rejects_empty_line() {
        assert_eq!(parse_line("   "), Err(ParseError::Empty));
    }

    #[test]
    fn rejects_unknown_command() {
        assert_eq!(
            parse_line("hold TESTE/BRL 100 1"),
            Err(ParseError::UnknownCommand("hold".to_string()))
        );
    }

    #[test]
    fn rejects_wrong_number_of_arguments() {
        assert_eq!(parse_line("buy TESTE/BRL 100"), Err(ParseError::WrongArity));
        assert_eq!(
            parse_line("buy TESTE/BRL 100 5 gtc 9"),
            Err(ParseError::WrongArity)
        );
        assert_eq!(parse_line("quit now"), Err(ParseError::WrongArity));
    }

    #[test]
    fn rejects_non_numeric_price_and_lots() {
        assert_eq!(
            parse_line("buy TESTE/BRL abc 5"),
            Err(ParseError::InvalidNumber("abc".to_string()))
        );
        assert_eq!(
            parse_line("buy TESTE/BRL 100 1.5"),
            Err(ParseError::InvalidNumber("1.5".to_string()))
        );
    }

    #[test]
    fn rejects_values_the_domain_refuses() {
        assert_eq!(
            parse_line("buy TESTE/BRL -5 5"),
            Err(ParseError::Domain(DomainError::NonPositivePrice(-5)))
        );
        assert_eq!(
            parse_line("sell TESTE/BRL 100 -1"),
            Err(ParseError::Domain(DomainError::NegativeLots(-1)))
        );
    }

    fn kind_of(line: &str) -> Result<OrderKind, ParseError> {
        parse_line(line).map(|input| match input {
            Input::Submit { kind, .. } => kind,
            other => panic!("esperava uma ordem, veio {other:?}"),
        })
    }

    #[test]
    fn parses_stop_market_and_stop_limit() {
        let price = |units| Price::new(units).expect("preço de teste válido");
        assert_eq!(
            kind_of("sell TESTE/BRL mkt 5 stop 9500"),
            Ok(OrderKind::Stop {
                trigger: price(9_500),
                limit: None
            })
        );
        assert_eq!(
            kind_of("sell TESTE/BRL 9400 5 stop 9500 ioc"),
            Ok(OrderKind::Stop {
                trigger: price(9_500),
                limit: Some(price(9_400))
            })
        );
    }

    #[test]
    fn stop_with_protection_is_an_error() {
        assert_eq!(
            parse_line("sell TESTE/BRL mkt 5 max 9000 stop 9500"),
            Err(ParseError::StopTakesNoProtection)
        );
        assert_eq!(
            parse_line("sell TESTE/BRL mkt 5 stop 9500 max 9000"),
            Err(ParseError::StopTakesNoProtection)
        );
    }

    #[test]
    fn stop_trigger_must_be_a_price() {
        assert_eq!(
            parse_line("buy TESTE/BRL mkt 5 stop abc"),
            Err(ParseError::InvalidNumber("abc".to_string()))
        );
        assert!(matches!(
            parse_line("buy TESTE/BRL mkt 5 stop 0"),
            Err(ParseError::Domain(_))
        ));
    }
}
