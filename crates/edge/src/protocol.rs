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
use crate::market_data::{Body, Feed as CoreFeed, Quote, Snapshot, Update};
use crate::sequencer::Intent;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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

impl From<CoreSide> for Side {
    fn from(side: CoreSide) -> Self {
        match side {
            CoreSide::Bid => Self::Buy,
            CoreSide::Ask => Self::Sell,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feed {
    L1,
    L2,
}

impl From<Feed> for CoreFeed {
    fn from(feed: Feed) -> Self {
        match feed {
            Feed::L1 => Self::L1,
            Feed::L2 => Self::L2,
        }
    }
}

impl From<CoreFeed> for Feed {
    fn from(feed: CoreFeed) -> Self {
        match feed {
            CoreFeed::L1 => Self::L1,
            CoreFeed::L2 => Self::L2,
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
    /// Dormente até o último negócio chegar a `trigger`; aí entra como limite em `limit`
    /// ou, sem ele, a mercado. Tipo próprio, e não um campo em `limit`/`market`: um stop
    /// não tem proteção explícita, e o `deny_unknown_fields` recusa a mistura.
    Stop {
        instrument: String,
        side: Side,
        trigger: Price,
        #[serde(default)]
        limit: Option<Price>,
        lots: Lots,
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
    /// Pública: vale antes do login. Assinar de novo o mesmo fluxo devolve um snapshot novo,
    /// que é como o cliente se recupera de um buraco na sequência.
    Subscribe {
        instrument: String,
        feed: Feed,
    },
}

/// O que a sessão faz com uma mensagem já validada e com nomes traduzidos em ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Inbound {
    Login(AccountId),
    Balance,
    Submit(Intent),
    Subscribe {
        instrument: InstrumentId,
        feed: CoreFeed,
    },
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
            Self::Subscribe { instrument, feed } => {
                let (instrument, feed) = subscription(instrument, feed)?;
                return Ok(Inbound::Subscribe { instrument, feed });
            }
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
            Self::Stop {
                instrument,
                side,
                trigger,
                limit,
                lots,
                tif,
            } => Intent::Order {
                account,
                instrument: known_instrument(instrument)?,
                side: side.into(),
                kind: OrderKind::Stop { trigger, limit },
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

/// O alvo de uma assinatura, sem precisar de conta: o feed é público.
///
/// # Errors
/// [`ProtocolError::UnknownInstrument`] se o instrumento não existir.
pub fn subscription(
    instrument: String,
    feed: Feed,
) -> Result<(InstrumentId, CoreFeed), ProtocolError> {
    Ok((known_instrument(instrument)?, feed.into()))
}

fn known_instrument(name: String) -> Result<InstrumentId, ProtocolError> {
    instrument_id(&name).ok_or(ProtocolError::UnknownInstrument(name))
}

fn known_asset(name: String) -> Result<AssetId, ProtocolError> {
    asset_id(&name).ok_or(ProtocolError::UnknownAsset(name))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Level {
    pub price: Price,
    pub lots: Lots,
}

impl From<Quote> for Level {
    fn from(quote: Quote) -> Self {
        Self {
            price: quote.price,
            lots: quote.lots,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AssetBalance {
    pub asset: String,
    pub available: Amount,
    pub reserved: Amount,
}

/// Mensagem do servidor. `code` dos rejeitados é estável, para máquina; `message` é texto
/// para gente e pode mudar.
///
/// É o mesmo tipo dos dois lados: o servidor serializa, o cliente desserializa. Ao contrário
/// de [`ClientMessage`], campo desconhecido é aceito: o servidor é rigoroso com o que recebe,
/// o cliente é tolerante com o que lê, para que campos novos não quebrem clientes antigos.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    LoggedIn {
        account: AccountId,
    },
    Deposited {
        asset: String,
        amount: Amount,
    },
    DepositRejected {
        asset: String,
        amount: Amount,
    },
    Fill {
        instrument: String,
        taker: OrderId,
        maker: OrderId,
        lots: Lots,
        price: Price,
        fee: Amount,
    },
    Rested {
        instrument: String,
        order: OrderId,
        lots: Lots,
    },
    Cancelled {
        instrument: String,
        order: OrderId,
        lots: Lots,
    },
    Expired {
        instrument: String,
        order: OrderId,
        lots: Lots,
    },
    Rejected {
        order: OrderId,
        code: String,
        message: String,
    },
    /// Stop aceito, dormente até o último negócio chegar ao gatilho.
    Armed {
        instrument: String,
        order: OrderId,
        side: Side,
        trigger: Price,
        lots: Lots,
    },
    /// O stop disparou; os eventos dele como ordem comum vêm em seguida.
    Triggered {
        instrument: String,
        order: OrderId,
    },
    Balances {
        balances: Vec<AssetBalance>,
    },
    Error {
        message: String,
    },
    Snapshot {
        instrument: String,
        feed: Feed,
        seq: u64,
        bids: Vec<Level>,
        asks: Vec<Level>,
    },
    /// Nível 2: quantidade absoluta no preço; zero quer dizer que o nível sumiu.
    Depth {
        instrument: String,
        feed: Feed,
        seq: u64,
        side: Side,
        price: Price,
        lots: Lots,
    },
    /// Nível 1: o topo do livro, `null` no lado vazio.
    Top {
        instrument: String,
        feed: Feed,
        seq: u64,
        bid: Option<Level>,
        ask: Option<Level>,
    },
    Trade {
        instrument: String,
        feed: Feed,
        seq: u64,
        price: Price,
        lots: Lots,
    },
}

/// Uma mensagem do feed traduzida de volta para os tipos do [`crate::market_data`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FeedMessage {
    Snapshot(Snapshot),
    Update(Update),
}

impl ServerMessage {
    /// `None` se não for mensagem do feed ou se o instrumento não existir no mercado.
    pub fn into_feed(self) -> Option<FeedMessage> {
        let quote = |level: Level| Quote {
            price: level.price,
            lots: level.lots,
        };
        let update = |instrument: &str, feed: Feed, seq, body| {
            Some(FeedMessage::Update(Update {
                instrument: instrument_id(instrument)?,
                feed: feed.into(),
                seq,
                body,
            }))
        };
        match self {
            Self::Snapshot {
                instrument,
                feed,
                seq,
                bids,
                asks,
            } => Some(FeedMessage::Snapshot(Snapshot {
                instrument: instrument_id(&instrument)?,
                feed: feed.into(),
                seq,
                bids: bids.into_iter().map(quote).collect(),
                asks: asks.into_iter().map(quote).collect(),
            })),
            Self::Depth {
                instrument,
                feed,
                seq,
                side,
                price,
                lots,
            } => update(
                &instrument,
                feed,
                seq,
                Body::Level {
                    side: side.into(),
                    price,
                    lots,
                },
            ),
            Self::Top {
                instrument,
                feed,
                seq,
                bid,
                ask,
            } => update(
                &instrument,
                feed,
                seq,
                Body::Top {
                    bid: bid.map(quote),
                    ask: ask.map(quote),
                },
            ),
            Self::Trade {
                instrument,
                feed,
                seq,
                price,
                lots,
            } => update(&instrument, feed, seq, Body::Trade { price, lots }),
            _ => None,
        }
    }
}

impl From<&Snapshot> for ServerMessage {
    fn from(snapshot: &Snapshot) -> Self {
        let levels = |quotes: &[Quote]| quotes.iter().copied().map(Level::from).collect();
        Self::Snapshot {
            instrument: symbol(snapshot.instrument).to_string(),
            feed: snapshot.feed.into(),
            seq: snapshot.seq,
            bids: levels(&snapshot.bids),
            asks: levels(&snapshot.asks),
        }
    }
}

impl From<&Update> for ServerMessage {
    fn from(update: &Update) -> Self {
        let instrument = symbol(update.instrument).to_string();
        let feed = update.feed.into();
        let seq = update.seq;
        match update.body {
            Body::Level { side, price, lots } => Self::Depth {
                instrument,
                feed,
                seq,
                side: side.into(),
                price,
                lots,
            },
            Body::Top { bid, ask } => Self::Top {
                instrument,
                feed,
                seq,
                bid: bid.map(Level::from),
                ask: ask.map(Level::from),
            },
            Body::Trade { price, lots } => Self::Trade {
                instrument,
                feed,
                seq,
                price,
                lots,
            },
        }
    }
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
                asset: asset_name(asset).to_string(),
                amount,
            },
            Event::DepositRejected { asset, amount, .. } => Self::DepositRejected {
                asset: asset_name(asset).to_string(),
                amount,
            },
            Event::Fill {
                instrument,
                fill,
                fee,
                ..
            } => Self::Fill {
                instrument: symbol(instrument).to_string(),
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
                ..
            } => Self::Rested {
                instrument: symbol(instrument).to_string(),
                order,
                lots,
            },
            Event::Cancelled {
                instrument,
                order,
                lots,
                ..
            } => Self::Cancelled {
                instrument: symbol(instrument).to_string(),
                order,
                lots,
            },
            Event::Expired {
                instrument,
                order,
                lots,
                ..
            } => Self::Expired {
                instrument: symbol(instrument).to_string(),
                order,
                lots,
            },
            Event::Rejected { order, reason, .. } => Self::Rejected {
                order,
                code: reject_code(&reason).to_string(),
                message: reject_message(&reason),
            },
            Event::Armed {
                instrument,
                order,
                side,
                trigger,
                lots,
                ..
            } => Self::Armed {
                instrument: symbol(instrument).to_string(),
                order,
                side: side.into(),
                trigger,
                lots,
            },
            Event::Triggered {
                instrument, order, ..
            } => Self::Triggered {
                instrument: symbol(instrument).to_string(),
                order,
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
        RejectReason::StopAlreadyTriggered { .. } => "stop_already_triggered",
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
        RejectReason::StopAlreadyTriggered { trigger, last } => format!(
            "gatilho {} já alcançado pelo último negócio ({})",
            trigger.get(),
            last.get()
        ),
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
    fn stop_resolves_to_a_dormant_order() {
        assert_eq!(
            resolved(
                r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":9500,"lots":5}"#
            ),
            Ok(Inbound::Submit(Intent::Order {
                account: ACCOUNT,
                instrument: TESTE_BRL,
                side: CoreSide::Ask,
                kind: OrderKind::Stop {
                    trigger: price(9_500),
                    limit: None
                },
                lots: lots(5),
                time_in_force: TimeInForce::Gtc
            }))
        );
        assert_eq!(
            resolved(
                r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":9500,"limit":9400,"lots":5,"tif":"ioc"}"#
            ),
            Ok(Inbound::Submit(Intent::Order {
                account: ACCOUNT,
                instrument: TESTE_BRL,
                side: CoreSide::Ask,
                kind: OrderKind::Stop {
                    trigger: price(9_500),
                    limit: Some(price(9_400))
                },
                lots: lots(5),
                time_in_force: TimeInForce::Ioc
            }))
        );
    }

    #[test]
    fn stop_without_trigger_or_with_protection_is_malformed() {
        malformed(r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","lots":5}"#);
        malformed(
            r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":9500,"protection":9000,"lots":5}"#,
        );
        malformed(r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":0,"lots":5}"#);
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
                fee: amount(30),
                taker_account: ACCOUNT,
                maker_account: AccountId::new(2)
            }),
            "{\"type\":\"fill\",\"instrument\":\"TESTE/BRL\",\"taker\":2,\"maker\":1,\"lots\":3,\"price\":10000,\"fee\":30}\n"
        );
        assert_eq!(
            line(&Event::Rested {
                instrument: TESTE_BRL,
                order: OrderId::new(1),
                account: ACCOUNT,
                side: CoreSide::Bid,
                price: price(10_000),
                lots: lots(5)
            }),
            "{\"type\":\"rested\",\"instrument\":\"TESTE/BRL\",\"order\":1,\"lots\":5}\n"
        );
        assert_eq!(
            line(&Event::Cancelled {
                instrument: TESTE_BRL,
                order: OrderId::new(2),
                account: ACCOUNT,
                lots: lots(2)
            }),
            "{\"type\":\"cancelled\",\"instrument\":\"TESTE/BRL\",\"order\":2,\"lots\":2}\n"
        );
        assert_eq!(
            line(&Event::Expired {
                instrument: TESTE_BRL,
                order: OrderId::new(4),
                account: ACCOUNT,
                lots: lots(5)
            }),
            "{\"type\":\"expired\",\"instrument\":\"TESTE/BRL\",\"order\":4,\"lots\":5}\n"
        );
    }

    #[test]
    fn rejection_carries_a_stable_code_and_a_human_message() {
        let event = Event::Rejected {
            order: OrderId::new(4),
            account: ACCOUNT,
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
                asset: "BRL".to_string(),
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

    #[test]
    fn subscribe_is_decoded_and_resolved_without_an_account() {
        let line = r#"{"type":"subscribe","instrument":"TESTE/BRL","feed":"l2"}"#;

        assert_eq!(
            decode(line),
            Ok(ClientMessage::Subscribe {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2
            })
        );
        assert_eq!(
            subscription("TESTE/BRL".to_string(), Feed::L1),
            Ok((TESTE_BRL, CoreFeed::L1))
        );
        assert_eq!(
            resolved(line),
            Ok(Inbound::Subscribe {
                instrument: TESTE_BRL,
                feed: CoreFeed::L2
            })
        );
        malformed(r#"{"type":"subscribe","instrument":"TESTE/BRL","feed":"l3"}"#);
        malformed(r#"{"type":"subscribe","instrument":"TESTE/BRL"}"#);
    }

    #[test]
    fn feed_messages_serialize_as_documented() {
        let snapshot = Snapshot {
            instrument: TESTE_BRL,
            feed: CoreFeed::L2,
            seq: 41,
            bids: vec![Quote {
                price: price(9_900),
                lots: lots(3),
            }],
            asks: vec![],
        };
        assert_eq!(
            ServerMessage::from(&snapshot).to_line(),
            "{\"type\":\"snapshot\",\"instrument\":\"TESTE/BRL\",\"feed\":\"l2\",\"seq\":41,\
             \"bids\":[{\"price\":9900,\"lots\":3}],\"asks\":[]}\n"
        );
        let update = |seq, feed, body| Update {
            instrument: TESTE_BRL,
            feed,
            seq,
            body,
        };
        assert_eq!(
            ServerMessage::from(&update(
                42,
                CoreFeed::L2,
                Body::Level {
                    side: CoreSide::Ask,
                    price: price(10_000),
                    lots: Lots::ZERO
                }
            ))
            .to_line(),
            "{\"type\":\"depth\",\"instrument\":\"TESTE/BRL\",\"feed\":\"l2\",\"seq\":42,\
             \"side\":\"sell\",\"price\":10000,\"lots\":0}\n"
        );
        assert_eq!(
            ServerMessage::from(&update(
                7,
                CoreFeed::L1,
                Body::Top {
                    bid: Some(Quote {
                        price: price(9_900),
                        lots: lots(3)
                    }),
                    ask: None
                }
            ))
            .to_line(),
            "{\"type\":\"top\",\"instrument\":\"TESTE/BRL\",\"feed\":\"l1\",\"seq\":7,\
             \"bid\":{\"price\":9900,\"lots\":3},\"ask\":null}\n"
        );
        assert_eq!(
            ServerMessage::from(&update(
                43,
                CoreFeed::L2,
                Body::Trade {
                    price: price(10_000),
                    lots: lots(2)
                }
            ))
            .to_line(),
            "{\"type\":\"trade\",\"instrument\":\"TESTE/BRL\",\"feed\":\"l2\",\"seq\":43,\
             \"price\":10000,\"lots\":2}\n"
        );
    }
}
