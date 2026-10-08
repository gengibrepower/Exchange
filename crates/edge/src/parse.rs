use std::str::FromStr;

use exchange_core::domain::{DomainError, Lots, OrderId, Price, Side};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    Submit {
        side: Side,
        symbol: String,
        price: Price,
        lots: Lots,
    },
    Cancel {
        symbol: String,
        order: OrderId,
    },
    Quit,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ParseError {
    Empty,
    UnknownCommand(String),
    WrongArity,
    InvalidNumber(String),
    Domain(DomainError),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "linha vazia"),
            Self::UnknownCommand(command) => write!(f, "comando desconhecido \"{command}\""),
            Self::WrongArity => write!(
                f,
                "uso: buy|sell <instrumento> <preço> <lots> | cancel <instrumento> <id> | quit"
            ),
            Self::InvalidNumber(token) => write!(f, "número inválido \"{token}\""),
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
        ["buy", symbol, price, lots] => submit(Side::Bid, symbol, price, lots),
        ["sell", symbol, price, lots] => submit(Side::Ask, symbol, price, lots),
        ["cancel", symbol, id] => Ok(Input::Cancel {
            symbol: symbol.to_string(),
            order: OrderId::new(number(id)?),
        }),
        ["buy" | "sell" | "cancel" | "quit", ..] => Err(ParseError::WrongArity),
        [command, ..] => Err(ParseError::UnknownCommand(command.to_string())),
    }
}

fn submit(side: Side, symbol: &str, price: &str, lots: &str) -> Result<Input, ParseError> {
    let price = Price::new(number(price)?).map_err(ParseError::Domain)?;
    let lots = Lots::new(number(lots)?).map_err(ParseError::Domain)?;
    Ok(Input::Submit {
        side,
        symbol: symbol.to_string(),
        price,
        lots,
    })
}

fn number<T: FromStr>(token: &str) -> Result<T, ParseError> {
    token
        .parse()
        .map_err(|_| ParseError::InvalidNumber(token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit(side: Side, price: i64, lots: i64) -> Input {
        Input::Submit {
            side,
            symbol: "TESTE/BRL".to_string(),
            price: Price::new(price).expect("preço de teste válido"),
            lots: Lots::new(lots).expect("lots de teste válidos"),
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
            parse_line("buy TESTE/BRL 100 5 9"),
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
}
