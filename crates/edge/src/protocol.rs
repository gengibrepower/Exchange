//! A tradução entre o fio ([`exchange_protocol`]) e o núcleo, só nesta fronteira
//! (seção 4.4). Os números chegam crus e viram newtypes aqui: é este o saneamento antes do
//! núcleo, e o que o domínio recusa (preço zero, lots negativo) volta como mensagem
//! malformada.
//!
//! Funções livres, e não `From`: os dois lados de `Side`, `Feed` e `Tif` são de outras
//! crates, e a regra do órfão proíbe implementar um trait de fora entre tipos de fora.

use std::fmt;

use exchange_core::domain::{
    AccountId, Amount, AssetId, DomainError, InstrumentId, Lots, OrderId, OrderKind, Price,
    Side as CoreSide, TimeInForce,
};
use exchange_core::engine::{Event, RejectReason};
use exchange_protocol::{
    ClientMessage, Feed, InstrumentSpec, Level, OpenOrder, OrderState, OrderType, Role,
    ServerMessage, Side, Tif,
};

use crate::core_thread::AccountOrder;
use crate::market::{asset_id, asset_name, instrument_id, listings, symbol};
use crate::market_data::{Body, Feed as CoreFeed, Quote, Snapshot, Update};
use crate::sequencer::Intent;

pub fn core_side(side: Side) -> CoreSide {
    match side {
        Side::Buy => CoreSide::Bid,
        Side::Sell => CoreSide::Ask,
    }
}

pub fn wire_side(side: CoreSide) -> Side {
    match side {
        CoreSide::Bid => Side::Buy,
        CoreSide::Ask => Side::Sell,
    }
}

pub fn core_feed(feed: Feed) -> CoreFeed {
    match feed {
        Feed::L1 => CoreFeed::L1,
        Feed::L2 => CoreFeed::L2,
    }
}

pub fn wire_feed(feed: CoreFeed) -> Feed {
    match feed {
        CoreFeed::L1 => Feed::L1,
        CoreFeed::L2 => Feed::L2,
    }
}

fn time_in_force(tif: Tif) -> TimeInForce {
    match tif {
        Tif::Gtc => TimeInForce::Gtc,
        Tif::Ioc => TimeInForce::Ioc,
        Tif::Fok => TimeInForce::Fok,
    }
}

fn wire_tif(time_in_force: TimeInForce) -> Tif {
    match time_in_force {
        TimeInForce::Gtc => Tif::Gtc,
        TimeInForce::Ioc => Tif::Ioc,
        TimeInForce::Fok => Tif::Fok,
    }
}

/// O que a sessão faz com uma mensagem já validada e com nomes traduzidos em ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Inbound {
    Login(AccountId),
    Balance,
    Instruments,
    Orders,
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

impl From<DomainError> for ProtocolError {
    fn from(error: DomainError) -> Self {
        Self::Malformed(error.to_string())
    }
}

/// # Errors
/// [`ProtocolError::Malformed`] se a linha não for exatamente uma [`ClientMessage`]:
/// JSON quebrado, `type` ou campo desconhecido, campo faltando, número com casas decimais.
pub fn decode(line: &str) -> Result<ClientMessage, ProtocolError> {
    ClientMessage::from_line(line).map_err(|error| ProtocolError::Malformed(error.to_string()))
}

/// Traduz nomes em ids, números em newtypes e monta a intenção para a conta da sessão.
///
/// # Errors
/// [`ProtocolError::Malformed`] se um número for um valor que o domínio recusa (preço zero,
/// lots ou quantia negativos); [`ProtocolError::UnknownInstrument`] ou
/// [`ProtocolError::UnknownAsset`] se um nome não existir no mercado.
pub fn resolve(message: ClientMessage, account: AccountId) -> Result<Inbound, ProtocolError> {
    let intent = match message {
        ClientMessage::Login { account } => return Ok(Inbound::Login(AccountId::new(account))),
        ClientMessage::Balance {} => return Ok(Inbound::Balance),
        ClientMessage::Instruments {} => return Ok(Inbound::Instruments),
        ClientMessage::Orders {} => return Ok(Inbound::Orders),
        ClientMessage::Subscribe { instrument, feed } => {
            let (instrument, feed) = subscription(instrument, feed)?;
            return Ok(Inbound::Subscribe { instrument, feed });
        }
        ClientMessage::Deposit { asset, amount } => Intent::Deposit {
            account,
            asset: known_asset(asset)?,
            amount: Amount::new(amount)?,
        },
        ClientMessage::Limit {
            instrument,
            side,
            price,
            lots,
            tif,
        } => Intent::Order {
            account,
            instrument: known_instrument(instrument)?,
            side: core_side(side),
            kind: OrderKind::Limit(Price::new(price)?),
            lots: Lots::new(lots)?,
            time_in_force: time_in_force(tif),
        },
        ClientMessage::Market {
            instrument,
            side,
            lots,
            protection,
            tif,
        } => Intent::Order {
            account,
            instrument: known_instrument(instrument)?,
            side: core_side(side),
            kind: OrderKind::Market {
                protection: protection.map(Price::new).transpose()?,
            },
            lots: Lots::new(lots)?,
            time_in_force: time_in_force(tif),
        },
        ClientMessage::Stop {
            instrument,
            side,
            trigger,
            limit,
            lots,
            tif,
        } => Intent::Order {
            account,
            instrument: known_instrument(instrument)?,
            side: core_side(side),
            kind: OrderKind::Stop {
                trigger: Price::new(trigger)?,
                limit: limit.map(Price::new).transpose()?,
            },
            lots: Lots::new(lots)?,
            time_in_force: time_in_force(tif),
        },
        ClientMessage::Cancel { instrument, order } => Intent::Cancel {
            account,
            instrument: known_instrument(instrument)?,
            order: OrderId::new(order),
        },
    };
    Ok(Inbound::Submit(intent))
}

/// O alvo de uma assinatura, sem precisar de conta: o feed é público.
///
/// # Errors
/// [`ProtocolError::UnknownInstrument`] se o instrumento não existir.
pub fn subscription(
    instrument: String,
    feed: Feed,
) -> Result<(InstrumentId, CoreFeed), ProtocolError> {
    Ok((known_instrument(instrument)?, core_feed(feed)))
}

/// A especificação de cada instrumento, tirada da mesma configuração que monta o engine.
pub fn instruments() -> ServerMessage {
    let instruments = listings()
        .iter()
        .map(|listing| {
            let instrument = listing.instrument;
            InstrumentSpec {
                symbol: symbol(instrument.id()).to_string(),
                base: asset_name(instrument.base()).to_string(),
                quote: asset_name(instrument.quote()).to_string(),
                tick: instrument.tick().get(),
                lot_size: instrument.lot_size().get(),
                max_lots: listing.risk.max_lots.get(),
            }
        })
        .collect();
    ServerMessage::Instruments { instruments }
}

pub fn open_orders(orders: &[AccountOrder]) -> ServerMessage {
    let orders = orders
        .iter()
        .map(|open| {
            let request = open.order.request;
            let (kind, price, trigger) = match request.kind {
                OrderKind::Limit(price) => (OrderType::Limit, Some(price), None),
                OrderKind::Market { .. } => (OrderType::Market, None, None),
                OrderKind::Stop { trigger, limit } => (OrderType::Stop, limit, Some(trigger)),
            };
            OpenOrder {
                instrument: symbol(request.instrument).to_string(),
                order: request.id.get(),
                side: wire_side(request.side),
                kind,
                price: price.map(Price::get),
                trigger: trigger.map(Price::get),
                tif: wire_tif(request.time_in_force),
                lots: request.total.get(),
                remaining: open.order.remaining.get(),
                state: if open.armed {
                    OrderState::Armed
                } else {
                    OrderState::Resting
                },
            }
        })
        .collect();
    ServerMessage::Orders { orders }
}

fn known_instrument(name: String) -> Result<InstrumentId, ProtocolError> {
    instrument_id(&name).ok_or(ProtocolError::UnknownInstrument(name))
}

fn known_asset(name: String) -> Result<AssetId, ProtocolError> {
    asset_id(&name).ok_or(ProtocolError::UnknownAsset(name))
}

fn wire_level(quote: Quote) -> Level {
    Level {
        price: quote.price.get(),
        lots: quote.lots.get(),
    }
}

/// Uma mensagem do feed traduzida de volta para os tipos do [`crate::market_data`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FeedMessage {
    Snapshot(Snapshot),
    Update(Update),
}

impl FeedMessage {
    /// `None` se não for mensagem do feed, se o instrumento não existir no mercado ou se um
    /// número for um valor que o domínio recusa.
    pub fn from_wire(message: ServerMessage) -> Option<Self> {
        let quote = |level: Level| {
            Some(Quote {
                price: Price::new(level.price).ok()?,
                lots: Lots::new(level.lots).ok()?,
            })
        };
        let optional = |level: Option<Level>| match level {
            None => Some(None),
            Some(level) => quote(level).map(Some),
        };
        let update = |instrument: &str, feed, seq, body| {
            Some(Self::Update(Update {
                instrument: instrument_id(instrument)?,
                feed: core_feed(feed),
                seq,
                body,
            }))
        };
        match message {
            ServerMessage::Snapshot {
                instrument,
                feed,
                seq,
                bids,
                asks,
            } => Some(Self::Snapshot(Snapshot {
                instrument: instrument_id(&instrument)?,
                feed: core_feed(feed),
                seq,
                bids: bids.into_iter().map(quote).collect::<Option<_>>()?,
                asks: asks.into_iter().map(quote).collect::<Option<_>>()?,
            })),
            ServerMessage::Depth {
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
                    side: core_side(side),
                    price: Price::new(price).ok()?,
                    lots: Lots::new(lots).ok()?,
                },
            ),
            ServerMessage::Top {
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
                    bid: optional(bid)?,
                    ask: optional(ask)?,
                },
            ),
            ServerMessage::Trade {
                instrument,
                feed,
                seq,
                price,
                lots,
            } => update(
                &instrument,
                feed,
                seq,
                Body::Trade {
                    price: Price::new(price).ok()?,
                    lots: Lots::new(lots).ok()?,
                },
            ),
            _ => None,
        }
    }
}

impl From<&Snapshot> for ServerMessage {
    fn from(snapshot: &Snapshot) -> Self {
        let levels = |quotes: &[Quote]| quotes.iter().copied().map(wire_level).collect();
        Self::Snapshot {
            instrument: symbol(snapshot.instrument).to_string(),
            feed: wire_feed(snapshot.feed),
            seq: snapshot.seq,
            bids: levels(&snapshot.bids),
            asks: levels(&snapshot.asks),
        }
    }
}

impl From<&Update> for ServerMessage {
    fn from(update: &Update) -> Self {
        let instrument = symbol(update.instrument).to_string();
        let feed = wire_feed(update.feed);
        let seq = update.seq;
        match update.body {
            Body::Level { side, price, lots } => Self::Depth {
                instrument,
                feed,
                seq,
                side: wire_side(side),
                price: price.get(),
                lots: lots.get(),
            },
            Body::Top { bid, ask } => Self::Top {
                instrument,
                feed,
                seq,
                bid: bid.map(wire_level),
                ask: ask.map(wire_level),
            },
            Body::Trade { price, lots } => Self::Trade {
                instrument,
                feed,
                seq,
                price: price.get(),
                lots: lots.get(),
            },
        }
    }
}

/// O evento como `viewer` o vê; evento que não é dele não vira mensagem. Um fill vira
/// uma mensagem por ordem dele (as duas num autonegócio), com o papel e a taxa que ele
/// pagou: o maker não paga (decisão 7), e a ordem do outro lado não aparece.
pub fn from_event(viewer: AccountId, event: &Event) -> Vec<ServerMessage> {
    let message = match *event {
        Event::Deposited { asset, amount, .. } => ServerMessage::Deposited {
            asset: asset_name(asset).to_string(),
            amount: amount.get(),
        },
        Event::DepositRejected { asset, amount, .. } => ServerMessage::DepositRejected {
            asset: asset_name(asset).to_string(),
            amount: amount.get(),
        },
        Event::Fill {
            instrument,
            fill,
            fee,
            taker_account,
            maker_account,
        } => {
            let sides = [
                (taker_account, Role::Taker, fill.taker, fee),
                (maker_account, Role::Maker, fill.maker, Amount::ZERO),
            ];
            return sides
                .into_iter()
                .filter(|&(account, ..)| account == viewer)
                .map(|(_, role, order, fee)| ServerMessage::Fill {
                    instrument: symbol(instrument).to_string(),
                    role,
                    order: order.get(),
                    lots: fill.lots.get(),
                    price: fill.price.get(),
                    fee: fee.get(),
                })
                .collect();
        }
        Event::Rested {
            instrument,
            order,
            side,
            price,
            lots,
            ..
        } => ServerMessage::Rested {
            instrument: symbol(instrument).to_string(),
            order: order.get(),
            side: wire_side(side),
            price: price.get(),
            lots: lots.get(),
        },
        Event::Cancelled {
            instrument,
            order,
            lots,
            ..
        } => ServerMessage::Cancelled {
            instrument: symbol(instrument).to_string(),
            order: order.get(),
            lots: lots.get(),
        },
        Event::Expired {
            instrument,
            order,
            lots,
            ..
        } => ServerMessage::Expired {
            instrument: symbol(instrument).to_string(),
            order: order.get(),
            lots: lots.get(),
        },
        Event::Rejected { order, reason, .. } => ServerMessage::Rejected {
            order: order.get(),
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
        } => ServerMessage::Armed {
            instrument: symbol(instrument).to_string(),
            order: order.get(),
            side: wire_side(side),
            trigger: trigger.get(),
            lots: lots.get(),
        },
        Event::Triggered {
            instrument, order, ..
        } => ServerMessage::Triggered {
            instrument: symbol(instrument).to_string(),
            order: order.get(),
        },
    };
    if event.accounts().any(|account| account == viewer) {
        vec![message]
    } else {
        Vec::new()
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

    fn resolved(line: &str) -> Result<Inbound, ProtocolError> {
        resolve(decode(line).unwrap(), ACCOUNT)
    }

    fn refused(line: &str) -> String {
        match resolved(line) {
            Err(ProtocolError::Malformed(detail)) => detail,
            other => panic!("{line} deveria ser recusada, veio {other:?}"),
        }
    }

    #[test]
    fn broken_line_is_malformed() {
        assert!(matches!(
            decode(r#"{"type":"login","account":1"#),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn numbers_the_domain_refuses_are_malformed() {
        assert_eq!(
            refused(r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":0,"lots":5}"#),
            "preço 0 deve ser positivo"
        );
        assert_eq!(
            refused(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":-1}"#
            ),
            "lots -1 não pode ser negativo"
        );
        assert_eq!(
            refused(r#"{"type":"deposit","asset":"BRL","amount":-5}"#),
            "quantia -5 não pode ser negativa"
        );
        refused(
            r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":1,"protection":-1}"#,
        );
        refused(r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":0,"lots":5}"#);
        refused(
            r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":9500,"limit":0,"lots":5}"#,
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
        assert_eq!(
            resolved(r#"{"type":"instruments"}"#),
            Ok(Inbound::Instruments)
        );
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
        lines(ACCOUNT, event)
    }

    fn lines(viewer: AccountId, event: &Event) -> String {
        from_event(viewer, event)
            .iter()
            .map(ServerMessage::to_line)
            .collect()
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
            "{\"type\":\"fill\",\"instrument\":\"TESTE/BRL\",\"role\":\"taker\",\"order\":2,\"lots\":3,\"price\":10000,\"fee\":30}\n"
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
            "{\"type\":\"rested\",\"instrument\":\"TESTE/BRL\",\"order\":1,\"side\":\"buy\",\"price\":10000,\"lots\":5}\n"
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

    fn fill_between(taker_account: AccountId, maker_account: AccountId) -> Event {
        Event::Fill {
            instrument: TESTE_BRL,
            fill: Fill {
                taker: OrderId::new(2),
                maker: OrderId::new(1),
                price: price(10_000),
                lots: lots(3),
            },
            fee: amount(30),
            taker_account,
            maker_account,
        }
    }

    #[test]
    fn maker_sees_its_own_order_and_pays_no_fee() {
        assert_eq!(
            lines(AccountId::new(2), &fill_between(ACCOUNT, AccountId::new(2))),
            "{\"type\":\"fill\",\"instrument\":\"TESTE/BRL\",\"role\":\"maker\",\"order\":1,\"lots\":3,\"price\":10000,\"fee\":0}\n"
        );
    }

    #[test]
    fn self_trade_is_one_line_per_order() {
        assert_eq!(
            lines(ACCOUNT, &fill_between(ACCOUNT, ACCOUNT)),
            concat!(
                "{\"type\":\"fill\",\"instrument\":\"TESTE/BRL\",\"role\":\"taker\",\"order\":2,\"lots\":3,\"price\":10000,\"fee\":30}\n",
                "{\"type\":\"fill\",\"instrument\":\"TESTE/BRL\",\"role\":\"maker\",\"order\":1,\"lots\":3,\"price\":10000,\"fee\":0}\n"
            )
        );
    }

    #[test]
    fn events_of_another_account_become_nothing() {
        let stranger = AccountId::new(9);
        assert_eq!(
            lines(stranger, &fill_between(ACCOUNT, AccountId::new(2))),
            ""
        );
        let rested = Event::Rested {
            instrument: TESTE_BRL,
            order: OrderId::new(1),
            account: ACCOUNT,
            side: CoreSide::Bid,
            price: price(10_000),
            lots: lots(5),
        };
        assert_eq!(lines(stranger, &rested), "");
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
    fn subscribe_is_resolved_without_an_account() {
        let line = r#"{"type":"subscribe","instrument":"TESTE/BRL","feed":"l2"}"#;

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
            FeedMessage::from_wire(ServerMessage::from(&snapshot)),
            Some(FeedMessage::Snapshot(snapshot.clone()))
        );
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
