//! Protocolo da borda, fase 1 (seção 4.4): uma mensagem JSON por linha. Os tipos daqui são
//! o contrato com o cliente e mudam devagar; os do núcleo podem ser refatorados sem quebrar
//! ninguém, porque a tradução acontece só nesta fronteira.

use std::fmt;

use exchange_core::domain::{
    AccountId, Amount, AssetId, InstrumentId, Lots, OrderId, OrderKind, Price, Side as CoreSide,
    TimeInForce,
};
use exchange_core::engine::{Event, RejectReason};
use serde::{Deserialize, Serialize};

use crate::market::{asset_id, asset_name, instrument_id, symbol};
use crate::sequencer::Intent;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl From<Side> for CoreSide {
    fn from(side: Side) -> Self {
        match side {
            Side::Buy => Self::Bid,
            Side::Sell => Self::Ask,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tif {
    #[default]
    Gtc,
    Ioc,
    Fok,
}

impl From<Tif> for TimeInForce {
    fn from(tif: Tif) -> Self {
        match tif {
            Tif::Gtc => Self::Gtc,
            Tif::Ioc => Self::Ioc,
            Tif::Fok => Self::Fok,
        }
    }
}

/// Mensagem do cliente. `type` decide a variante e campo desconhecido é erro: nada é
/// adivinhado. Ordem limite e a mercado são tipos distintos para que um `price` esquecido
/// nunca transforme uma limite numa ordem a mercado.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Login {
        account: AccountId,
    },
    Deposit {
        asset: String,
        amount: Amount,
    },
    Limit {
        instrument: String,
        side: Side,
        price: Price,
        lots: Lots,
        #[serde(default)]
        tif: Tif,
    },
    Market {
        instrument: String,
        side: Side,
        lots: Lots,
        #[serde(default)]
        protection: Option<Price>,
        #[serde(default)]
        tif: Tif,
    },
    Cancel {
        instrument: String,
        order: OrderId,
    },
    /// Variante com chaves vazias, não unitária: só assim o `deny_unknown_fields` vale
    /// para ela, e `{"type":"balance","account":1}` é erro em vez de ter o campo ignorado.
    Balance {},
}

/// O que a sessão faz com uma mensagem já validada e com nomes traduzidos em ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Inbound {
    Login(AccountId),
    Balance,
    Submit(Intent),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProtocolError {
    Malformed(String),
    UnknownInstrument(String),
    UnknownAsset(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "mensagem inválida: {detail}"),
            Self::UnknownInstrument(name) => write!(f, "instrumento desconhecido \"{name}\""),
            Self::UnknownAsset(name) => write!(f, "ativo desconhecido \"{name}\""),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// # Errors
/// [`ProtocolError::Malformed`] se a linha não for exatamente uma [`ClientMessage`]:
/// JSON quebrado, `type` ou campo desconhecido, campo faltando, número com casas decimais
/// ou valor que o domínio recusa (preço zero, lots negativo).
pub fn decode(line: &str) -> Result<ClientMessage, ProtocolError> {
    serde_json::from_str(line).map_err(|error| ProtocolError::Malformed(error.to_string()))
}

impl ClientMessage {
    /// Traduz nomes em ids e monta a intenção para a conta da sessão.
    ///
    /// # Errors
    /// [`ProtocolError::UnknownInstrument`] ou [`ProtocolError::UnknownAsset`] se um nome
    /// não existir no mercado.
    pub fn resolve(self, account: AccountId) -> Result<Inbound, ProtocolError> {
        let intent = match self {
            Self::Login { account } => return Ok(Inbound::Login(account)),
            Self::Balance {} => return Ok(Inbound::Balance),
            Self::Deposit { asset, amount } => Intent::Deposit {
                account,
                asset: known_asset(asset)?,
                amount,
            },
            Self::Limit {
                instrument,
                side,
                price,
                lots,
                tif,
            } => Intent::Order {
                account,
                instrument: known_instrument(instrument)?,
                side: side.into(),
                kind: OrderKind::Limit(price),
                lots,
                time_in_force: tif.into(),
            },
            Self::Market {
                instrument,
                side,
                lots,
                protection,
                tif,
            } => Intent::Order {
                account,
                instrument: known_instrument(instrument)?,
                side: side.into(),
                kind: OrderKind::Market { protection },
                lots,
                time_in_force: tif.into(),
            },
            Self::Cancel { instrument, order } => Intent::Cancel {
                account,
                instrument: known_instrument(instrument)?,
                order,
            },
        };
        Ok(Inbound::Submit(intent))
    }
}

fn known_instrument(name: String) -> Result<InstrumentId, ProtocolError> {
    instrument_id(&name).ok_or(ProtocolError::UnknownInstrument(name))
}

fn known_asset(name: String) -> Result<AssetId, ProtocolError> {
    asset_id(&name).ok_or(ProtocolError::UnknownAsset(name))
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct AssetBalance {
    pub asset: &'static str,
    pub available: Amount,
    pub reserved: Amount,
}

/// Mensagem do servidor. `code` dos rejeitados é estável, para máquina; `message` é texto
/// para gente e pode mudar.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    LoggedIn {
        account: AccountId,
    },
    Deposited {
        asset: &'static str,
        amount: Amount,
    },
    DepositRejected {
        asset: &'static str,
        amount: Amount,
    },
    Fill {
        instrument: &'static str,
        taker: OrderId,
        maker: OrderId,
        lots: Lots,
        price: Price,
        fee: Amount,
    },
    Rested {
        instrument: &'static str,
        order: OrderId,
        lots: Lots,
    },
    Cancelled {
        instrument: &'static str,
        order: OrderId,
        lots: Lots,
    },
    Expired {
        instrument: &'static str,
        order: OrderId,
        lots: Lots,
    },
    Rejected {
        order: OrderId,
        code: &'static str,
        message: String,
    },
    Balances {
        balances: Vec<AssetBalance>,
    },
    Error {
        message: String,
    },
}

impl ServerMessage {
    /// # Panics
    /// Nunca: a mensagem só tem números, textos e enums, que o `serde_json` sempre
    /// serializa.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("mensagem só tem números e textos");
        line.push('\n');
        line
    }
}

impl From<&Event> for ServerMessage {
    fn from(event: &Event) -> Self {
        match *event {
            Event::Deposited { asset, amount, .. } => Self::Deposited {
                asset: asset_name(asset),
                amount,
            },
            Event::DepositRejected { asset, amount, .. } => Self::DepositRejected {
                asset: asset_name(asset),
                amount,
            },
            Event::Fill {
                instrument,
                fill,
                fee,
            } => Self::Fill {
                instrument: symbol(instrument),
                taker: fill.taker,
                maker: fill.maker,
                lots: fill.lots,
                price: fill.price,
                fee,
            },
            Event::Rested {
                instrument,
                order,
                lots,
            } => Self::Rested {
                instrument: symbol(instrument),
                order,
                lots,
            },
            Event::Cancelled {
                instrument,
                order,
                lots,
            } => Self::Cancelled {
                instrument: symbol(instrument),
                order,
                lots,
            },
            Event::Expired {
                instrument,
                order,
                lots,
            } => Self::Expired {
                instrument: symbol(instrument),
                order,
                lots,
            },
            Event::Rejected { order, reason } => Self::Rejected {
                order,
                code: reject_code(&reason),
                message: reject_message(&reason),
            },
        }
    }
}

pub fn reject_code(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::UnknownInstrument(_) => "unknown_instrument",
        RejectReason::EmptyOrder => "empty_order",
        RejectReason::OffTick { .. } => "off_tick",
        RejectReason::DuplicateOrderId => "duplicate_order_id",
        RejectReason::HouseCannotTrade => "house_cannot_trade",
        RejectReason::OrderTooLarge { .. } => "order_too_large",
        RejectReason::OutsideCollar { .. } => "outside_collar",
        RejectReason::NoReferencePrice => "no_reference_price",
        RejectReason::ValueOverflow => "value_overflow",
        RejectReason::InsufficientFunds { .. } => "insufficient_funds",
        RejectReason::UnknownOrder => "unknown_order",
        RejectReason::NotOwner => "not_owner",
    }
}

pub fn reject_message(reason: &RejectReason) -> String {
    match reason {
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
    }
}

#[cfg(test)]
mod tests {
    use exchange_core::domain::{AssetId, Fill, InstrumentId};

    use super::*;
    use crate::market::{BRL, INSTRUMENTS, TESTE};

    const ACCOUNT: AccountId = AccountId::new(7);
    const TESTE_BRL: InstrumentId = INSTRUMENTS[0].1;

    fn price(value: i64) -> Price {
        Price::new(value).unwrap()
    }

    fn lots(value: i64) -> Lots {
        Lots::new(value).unwrap()
    }

    fn amount(value: i64) -> Amount {
        Amount::new(value).unwrap()
    }

    fn malformed(line: &str) -> String {
        match decode(line) {
            Err(ProtocolError::Malformed(detail)) => detail,
            other => panic!("{line} deveria ser malformada, veio {other:?}"),
        }
    }

    fn resolved(line: &str) -> Result<Inbound, ProtocolError> {
        decode(line).unwrap().resolve(ACCOUNT)
    }

    #[test]
    fn decodes_every_client_message() {
        assert_eq!(
            decode(r#"{"type":"login","account":1}"#),
            Ok(ClientMessage::Login {
                account: AccountId::new(1)
            })
        );
        assert_eq!(
            decode(r#"{"type":"deposit","asset":"BRL","amount":500}"#),
            Ok(ClientMessage::Deposit {
                asset: "BRL".to_string(),
                amount: amount(500)
            })
        );
        assert_eq!(
            decode(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":5,"tif":"ioc"}"#
            ),
            Ok(ClientMessage::Limit {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                price: price(10_000),
                lots: lots(5),
                tif: Tif::Ioc
            })
        );
        assert_eq!(
            decode(
                r#"{"type":"market","instrument":"TESTE/BRL","side":"sell","lots":2,"protection":9500}"#
            ),
            Ok(ClientMessage::Market {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Sell,
                lots: lots(2),
                protection: Some(price(9_500)),
                tif: Tif::Gtc
            })
        );
        assert_eq!(
            decode(r#"{"type":"cancel","instrument":"TESTE/BRL","order":3}"#),
            Ok(ClientMessage::Cancel {
                instrument: "TESTE/BRL".to_string(),
                order: OrderId::new(3)
            })
        );
        assert_eq!(
            decode(r#"{"type":"balance"}"#),
            Ok(ClientMessage::Balance {})
        );
    }

    #[test]
    fn optional_fields_take_their_defaults() {
        assert_eq!(
            decode(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":1}"#),
            Ok(ClientMessage::Market {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                lots: lots(1),
                protection: None,
                tif: Tif::Gtc
            })
        );
    }

    #[test]
    fn field_order_and_whitespace_do_not_matter() {
        assert_eq!(
            decode(r#"  { "account" : 1 , "type" : "login" }  "#),
            Ok(ClientMessage::Login {
                account: AccountId::new(1)
            })
        );
    }

    #[test]
    fn broken_json_is_malformed() {
        malformed(r#"{"type":"login","account":1"#);
        malformed("buy TESTE/BRL 10000 5");
        malformed("");
    }

    #[test]
    fn unknown_type_and_missing_type_are_malformed() {
        malformed(r#"{"type":"withdraw","asset":"BRL","amount":1}"#);
        malformed(r#"{"account":1}"#);
    }

    #[test]
    fn unknown_field_is_malformed_not_ignored() {
        let detail = malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lot":5}"#,
        );
        assert!(detail.contains("lot"), "{detail}");
        malformed(r#"{"type":"balance","account":1}"#);
    }

    #[test]
    fn limit_without_price_and_market_with_price_are_malformed() {
        malformed(r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","lots":5}"#);
        malformed(
            r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":5}"#,
        );
    }

    #[test]
    fn numbers_are_integers_the_domain_accepts() {
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":100.5,"lots":5}"#,
        );
        malformed(r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":0,"lots":5}"#);
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":-1}"#,
        );
        malformed(r#"{"type":"deposit","asset":"BRL","amount":-5}"#);
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":"10000","lots":5}"#,
        );
    }

    #[test]
    fn side_and_tif_use_the_wire_words() {
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"bid","price":10000,"lots":5}"#,
        );
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":5,"tif":"GTC"}"#,
        );
    }

    #[test]
    fn resolve_builds_intents_for_the_session_account() {
        assert_eq!(
            resolved(r#"{"type":"deposit","asset":"TESTE","amount":500}"#),
            Ok(Inbound::Submit(Intent::Deposit {
                account: ACCOUNT,
                asset: TESTE,
                amount: amount(500)
            }))
        );
        assert_eq!(
            resolved(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":5,"tif":"fok"}"#
            ),
            Ok(Inbound::Submit(Intent::Order {
                account: ACCOUNT,
                instrument: TESTE_BRL,
                side: CoreSide::Ask,
                kind: OrderKind::Limit(price(10_000)),
                lots: lots(5),
                time_in_force: TimeInForce::Fok
            }))
        );
        assert_eq!(
            resolved(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":2}"#),
            Ok(Inbound::Submit(Intent::Order {
                account: ACCOUNT,
                instrument: TESTE_BRL,
                side: CoreSide::Bid,
                kind: OrderKind::Market { protection: None },
                lots: lots(2),
                time_in_force: TimeInForce::Gtc
            }))
        );
        assert_eq!(
            resolved(r#"{"type":"cancel","instrument":"TESTE/BRL","order":3}"#),
            Ok(Inbound::Submit(Intent::Cancel {
                account: ACCOUNT,
                instrument: TESTE_BRL,
                order: OrderId::new(3)
            }))
        );
    }

    #[test]
    fn resolve_passes_login_and_balance_through() {
        assert_eq!(
            resolved(r#"{"type":"login","account":1}"#),
            Ok(Inbound::Login(AccountId::new(1)))
        );
        assert_eq!(resolved(r#"{"type":"balance"}"#), Ok(Inbound::Balance));
    }

    #[test]
    fn resolve_rejects_unknown_names() {
        assert_eq!(
            resolved(r#"{"type":"deposit","asset":"OURO","amount":1}"#),
            Err(ProtocolError::UnknownAsset("OURO".to_string()))
        );
        assert_eq!(
            resolved(r#"{"type":"limit","instrument":"BTC/BRL","side":"buy","price":10,"lots":1}"#),
            Err(ProtocolError::UnknownInstrument("BTC/BRL".to_string()))
        );
        assert_eq!(
            resolved(r#"{"type":"market","instrument":"teste/brl","side":"buy","lots":1}"#),
            Err(ProtocolError::UnknownInstrument("teste/brl".to_string()))
        );
        assert_eq!(
            resolved(r#"{"type":"cancel","instrument":"BTC/BRL","order":1}"#),
            Err(ProtocolError::UnknownInstrument("BTC/BRL".to_string()))
        );
    }

    fn line(event: &Event) -> String {
        ServerMessage::from(event).to_line()
    }

    #[test]
    fn events_become_one_json_line_each() {
        assert_eq!(
            line(&Event::Deposited {
                account: ACCOUNT,
                asset: BRL,
                amount: amount(500)
            }),
            "{\"type\":\"deposited\",\"asset\":\"BRL\",\"amount\":500}\n"
        );
        assert_eq!(
            line(&Event::DepositRejected {
                account: ACCOUNT,
                asset: TESTE,
                amount: amount(9)
            }),
            "{\"type\":\"deposit_rejected\",\"asset\":\"TESTE\",\"amount\":9}\n"
        );
        assert_eq!(
            line(&Event::Fill {
                instrument: TESTE_BRL,
                fill: Fill {
                    taker: OrderId::new(2),
                    maker: OrderId::new(1),
                    price: price(10_000),
                    lots: lots(3)
                },
                fee: amount(30)
            }),
            "{\"type\":\"fill\",\"instrument\":\"TESTE/BRL\",\"taker\":2,\"maker\":1,\"lots\":3,\"price\":10000,\"fee\":30}\n"
        );
        assert_eq!(
            line(&Event::Rested {
                instrument: TESTE_BRL,
                order: OrderId::new(1),
                lots: lots(5)
            }),
            "{\"type\":\"rested\",\"instrument\":\"TESTE/BRL\",\"order\":1,\"lots\":5}\n"
        );
        assert_eq!(
            line(&Event::Cancelled {
                instrument: TESTE_BRL,
                order: OrderId::new(2),
                lots: lots(2)
            }),
            "{\"type\":\"cancelled\",\"instrument\":\"TESTE/BRL\",\"order\":2,\"lots\":2}\n"
        );
        assert_eq!(
            line(&Event::Expired {
                instrument: TESTE_BRL,
                order: OrderId::new(4),
                lots: lots(5)
            }),
            "{\"type\":\"expired\",\"instrument\":\"TESTE/BRL\",\"order\":4,\"lots\":5}\n"
        );
    }

    #[test]
    fn rejection_carries_a_stable_code_and_a_human_message() {
        let event = Event::Rejected {
            order: OrderId::new(4),
            reason: RejectReason::InsufficientFunds {
                asset: AssetId::new(2),
                available: amount(0),
                required: amount(10_010),
            },
        };

        assert_eq!(
            line(&event),
            "{\"type\":\"rejected\",\"order\":4,\"code\":\"insufficient_funds\",\
             \"message\":\"saldo insuficiente em BRL: disponível 0, necessário 10010\"}\n"
        );
    }

    #[test]
    fn session_messages_serialize_as_documented() {
        assert_eq!(
            ServerMessage::LoggedIn { account: ACCOUNT }.to_line(),
            "{\"type\":\"logged_in\",\"account\":7}\n"
        );
        let balances = ServerMessage::Balances {
            balances: vec![AssetBalance {
                asset: "BRL",
                available: amount(10),
                reserved: amount(5),
            }],
        };
        assert_eq!(
            balances.to_line(),
            "{\"type\":\"balances\",\"balances\":[{\"asset\":\"BRL\",\"available\":10,\"reserved\":5}]}\n"
        );
        assert_eq!(
            ServerMessage::Error {
                message: "x".to_string()
            }
            .to_line(),
            "{\"type\":\"error\",\"message\":\"x\"}\n"
        );
    }

    #[test]
    fn message_text_with_quotes_stays_one_valid_line() {
        let error = ServerMessage::Error {
            message: "ativo desconhecido \"OU\nRO\"".to_string(),
        };

        let line = error.to_line();

        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }
}
