use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::book::{CancelError, Leftover, OrderBook};
use crate::domain::{
    AccountId, Amount, AssetId, Bps, Fill, Instrument, InstrumentId, Lots, Order, OrderId,
    OrderRequest, OrderStatus, Price, Side,
};
use crate::ledger::{Balance, Ledger, LedgerError, Transfer};

mod stops;

use stops::StopBook;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Command {
    Deposit {
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    },
    Submit(OrderRequest),
    Cancel {
        instrument: InstrumentId,
        account: AccountId,
        order: OrderId,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    Deposited {
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    },
    DepositRejected {
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    },
    Fill {
        instrument: InstrumentId,
        fill: Fill,
        fee: Amount,
    },
    /// A sobra de uma ordem limite que ficou no livro; lado e preço deixam quem só consome
    /// eventos (o feed de market data, seção 4.3) reconstruir o livro sem olhar o engine.
    Rested {
        instrument: InstrumentId,
        order: OrderId,
        side: Side,
        price: Price,
        lots: Lots,
    },
    Cancelled {
        instrument: InstrumentId,
        order: OrderId,
        lots: Lots,
    },
    Expired {
        instrument: InstrumentId,
        order: OrderId,
        lots: Lots,
    },
    Rejected {
        order: OrderId,
        reason: RejectReason,
    },
    /// Stop aceito e dormente: reserva feita, fora do livro e do feed.
    Armed {
        instrument: InstrumentId,
        order: OrderId,
        side: Side,
        trigger: Price,
        lots: Lots,
    },
    /// O último negócio chegou ao gatilho: o stop entra agora como ordem limite ou a
    /// mercado, e os eventos dele vêm logo depois deste.
    Triggered {
        instrument: InstrumentId,
        order: OrderId,
    },
}

impl Event {
    /// A ordem de que o evento fala; num fill, a do taker. `None` em depósitos.
    pub fn order(&self) -> Option<OrderId> {
        match *self {
            Self::Rested { order, .. }
            | Self::Cancelled { order, .. }
            | Self::Expired { order, .. }
            | Self::Rejected { order, .. }
            | Self::Armed { order, .. }
            | Self::Triggered { order, .. } => Some(order),
            Self::Fill { fill, .. } => Some(fill.taker),
            Self::Deposited { .. } | Self::DepositRejected { .. } => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    UnknownInstrument(InstrumentId),
    EmptyOrder,
    OffTick {
        price: Price,
        tick: Price,
    },
    DuplicateOrderId,
    HouseCannotTrade,
    OrderTooLarge {
        lots: Lots,
        max: Lots,
    },
    OutsideCollar {
        bound: Price,
        edge: Price,
    },
    NoReferencePrice,
    ValueOverflow,
    InsufficientFunds {
        asset: AssetId,
        available: Amount,
        required: Amount,
    },
    UnknownOrder,
    NotOwner,
    /// O gatilho já foi alcançado pelo último negócio: o stop dispararia ao nascer, o que
    /// quase sempre é lado ou preço trocado.
    StopAlreadyTriggered {
        trigger: Price,
        last: Price,
    },
}

/// Taxa de taker em bps para a conta da casa; maker isento (decisão 7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FeeSchedule {
    pub house: AccountId,
    pub taker_bps: Bps,
}

/// Política de risco pré-trade de um instrumento (seção 4.1). Separada do `Instrument`:
/// um descreve o produto, a outra quanto se deixa negociar dele.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RiskLimits {
    pub max_lots: Lots,
    /// Largura do colar de preço em bps sobre o melhor preço do lado oposto.
    pub collar: Bps,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListingConfig {
    pub instrument: Instrument,
    pub risk: RiskLimits,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    DuplicateInstrument(InstrumentId),
}

#[derive(PartialEq, Eq, Debug)]
struct Listing {
    instrument: Instrument,
    risk: RiskLimits,
    book: OrderBook,
    stops: StopBook,
    /// O preço do último negócio, que dispara os stops (decisão de F7: último negócio, não
    /// o topo do livro, que se move sem ninguém negociar).
    last_trade: Option<Price>,
}

/// O que uma ordem aberta ainda trava no ledger. Soma, por conta e ativo, igual ao
/// `reserved` do ledger — é a invariante "reserva consistente" da seção 3.5.
///
/// `per_lot` é quanto cada lot da ordem travou: `lot_size` de base para o vendedor;
/// `limite + taxa de 1 lot` de quote para o comprador. Cada fill consome `lots × per_lot`
/// e o que não foi gasto (melhoria de preço, taxa que o maker não paga) volta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Reservation {
    account: AccountId,
    asset: AssetId,
    amount: Amount,
    per_lot: Amount,
}

/// As três pernas de um negócio (seção 4.2): base do vendedor para o comprador, quote
/// do comprador para o vendedor e a taxa do comprador para a casa. Se o taker é o
/// vendedor, a taxa sai do que ele recebe: `quote` vem descontada.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FillSettlement {
    base: Transfer,
    quote: Transfer,
    fee: Transfer,
}

#[derive(Clone, Copy)]
struct Parties {
    buyer: AccountId,
    seller: AccountId,
    house: AccountId,
}

/// Igualdade é estado lógico idêntico (seção 3.5): é o que o replay do log promete
/// reconstruir, e o que os testes de recuperação comparam.
#[derive(PartialEq, Eq, Debug)]
pub struct Engine {
    listings: HashMap<InstrumentId, Listing>,
    ledger: Ledger,
    reservations: Reservations,
    fees: FeeSchedule,
}

impl Engine {
    /// # Errors
    /// [`ConfigError::DuplicateInstrument`] se o mesmo id aparecer duas vezes.
    pub fn new(
        listings: impl IntoIterator<Item = ListingConfig>,
        fees: FeeSchedule,
    ) -> Result<Self, ConfigError> {
        let configs = listings;
        let mut listings = HashMap::new();
        for config in configs {
            let id = config.instrument.id();
            let listing = Listing {
                instrument: config.instrument,
                risk: config.risk,
                book: OrderBook::new(),
                stops: StopBook::default(),
                last_trade: None,
            };
            if listings.insert(id, listing).is_some() {
                return Err(ConfigError::DuplicateInstrument(id));
            }
        }
        Ok(Self {
            listings,
            ledger: Ledger::new(),
            reservations: HashMap::new(),
            fees,
        })
    }

    pub fn instrument(&self, id: InstrumentId) -> Option<&Instrument> {
        self.listings.get(&id).map(|listing| &listing.instrument)
    }

    pub fn book(&self, instrument: InstrumentId) -> Option<&OrderBook> {
        self.listings.get(&instrument).map(|listing| &listing.book)
    }

    pub fn balance(&self, account: AccountId, asset: AssetId) -> Balance {
        self.ledger.balance(account, asset)
    }

    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        match command {
            Command::Deposit {
                account,
                asset,
                amount,
            } => {
                let event = match self.ledger.deposit(account, asset, amount) {
                    Ok(()) => Event::Deposited {
                        account,
                        asset,
                        amount,
                    },
                    Err(_) => Event::DepositRejected {
                        account,
                        asset,
                        amount,
                    },
                };
                vec![event]
            }
            Command::Submit(order) => self.submit(order),
            Command::Cancel {
                instrument,
                account,
                order,
            } => self.cancel(instrument, account, order),
        }
    }

    /// Aceita a ordem (arma o stop ou executa) e depois dispara os stops que o novo
    /// último negócio alcançou, um de cada vez, até nenhum mais ser alcançado: um stop
    /// disparado pode mover o preço e disparar outros (cascata). Tudo dentro do mesmo
    /// comando e derivado só do estado, então o replay reproduz exatamente.
    fn submit(&mut self, request: OrderRequest) -> Vec<Event> {
        let instrument = request.instrument;
        let mut events = self.accept(request);
        self.fire_stops(instrument, &mut events);
        events
    }

    /// # Panics
    /// Nunca por entrada: o aceite não falha, a ordem acabou de nascer do pedido e só pode
    /// estar em `New`.
    fn accept(&mut self, request: OrderRequest) -> Vec<Event> {
        let OrderRequest {
            id,
            account,
            instrument,
            ..
        } = request;
        let reject = |reason| vec![Event::Rejected { order: id, reason }];
        let Ok(order) = Order::from_request(request) else {
            return reject(RejectReason::EmptyOrder);
        };

        if account == self.fees.house {
            return reject(RejectReason::HouseCannotTrade);
        }

        let Some(listing) = self.listings.get_mut(&instrument) else {
            return reject(RejectReason::UnknownInstrument(instrument));
        };
        let spec = listing.instrument;

        if let Some(price) = order
            .quoted_prices()
            .into_iter()
            .flatten()
            .find(|&price| !spec.accepts_price(price))
        {
            return reject(RejectReason::OffTick {
                price,
                tick: spec.tick(),
            });
        }
        if listing.book.contains(id) || listing.stops.contains(id) {
            return reject(RejectReason::DuplicateOrderId);
        }

        if let Some(trigger) = order.trigger() {
            return match arm(
                listing,
                &mut self.ledger,
                &mut self.reservations,
                self.fees,
                order,
                trigger,
            ) {
                Ok(event) => vec![event],
                Err(reason) => reject(reason),
            };
        }

        let mut order = match pre_trade_risk(order, &listing.book, listing.risk) {
            Ok(order) => order,
            Err(reason) => return reject(reason),
        };
        order
            .transition(OrderStatus::Accepted)
            .expect("ordem recém-construída está em New, e New → Accepted é legal");
        if let Err(reason) = reserve(
            &mut self.ledger,
            &mut self.reservations,
            &spec,
            self.fees.taker_bps,
            &order,
            order.price_bound(),
        ) {
            return reject(reason);
        }
        execute(
            listing,
            &mut self.ledger,
            &mut self.reservations,
            self.fees,
            order,
        )
    }

    fn fire_stops(&mut self, instrument: InstrumentId, events: &mut Vec<Event>) {
        let Some(listing) = self.listings.get_mut(&instrument) else {
            return;
        };
        while let Some(last) = listing.last_trade
            && let Some((stop, trigger)) = listing.stops.pop_triggered(last)
        {
            events.push(Event::Triggered {
                instrument,
                order: stop.id(),
            });
            events.extend(fire(
                listing,
                &mut self.ledger,
                &mut self.reservations,
                self.fees,
                stop,
                trigger,
            ));
        }
    }

    fn cancel(
        &mut self,
        instrument: InstrumentId,
        account: AccountId,
        order: OrderId,
    ) -> Vec<Event> {
        let Some(listing) = self.listings.get_mut(&instrument) else {
            return vec![Event::Rejected {
                order,
                reason: RejectReason::UnknownInstrument(instrument),
            }];
        };
        let cancelled = match listing.book.cancel(order, account) {
            Err(CancelError::UnknownOrder) => listing.stops.cancel(order, account),
            other => other,
        };
        let event = match cancelled {
            Ok(cancelled) => {
                release_rest(
                    &mut self.ledger,
                    &mut self.reservations,
                    (instrument, order),
                );
                Event::Cancelled {
                    instrument,
                    order,
                    lots: cancelled.remaining(),
                }
            }
            Err(CancelError::UnknownOrder) => Event::Rejected {
                order,
                reason: RejectReason::UnknownOrder,
            },
            Err(CancelError::NotOwner) => Event::Rejected {
                order,
                reason: RejectReason::NotOwner,
            },
        };
        vec![event]
    }
}

type Reservations = HashMap<(InstrumentId, OrderId), Reservation>;

#[derive(Clone, Copy)]
struct Taker {
    id: OrderId,
    side: Side,
}

/// Liquida um fill: as três pernas no ledger; abate `lots × per_lot` da reserva de
/// comprador e vendedor; devolve ao comprador o que a reserva cobria e ele não gastou.
/// Devolve a taxa cobrada.
fn settle_fill(
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    spec: &Instrument,
    fees: FeeSchedule,
    taker: Taker,
    fill: &Fill,
) -> Amount {
    let instrument = spec.id();
    let (buyer_order, seller_order) = match taker.side {
        Side::Bid => (taker.id, fill.maker),
        Side::Ask => (fill.maker, taker.id),
    };
    let buyer = reservations[&(instrument, buyer_order)];
    let seller = reservations[&(instrument, seller_order)];
    let parties = Parties {
        buyer: buyer.account,
        seller: seller.account,
        house: fees.house,
    };
    let value = fill
        .price
        .times(fill.lots)
        .expect("não passa da reserva do comprador, calculada sem estourar");
    let fee = fees.taker_bps.fee_on(value);
    let settlement = fill_settlement(spec, fill, parties, fee, taker.side);
    ledger
        .settle(&[settlement.base, settlement.quote, settlement.fee])
        .expect("reserva cobre os débitos e oferta limitada impede estouro");

    let buyer_consumed = buyer
        .per_lot
        .times(fill.lots)
        .expect("parte da reserva do comprador, calculada sem estourar");
    let buyer_paid = settlement
        .quote
        .amount
        .checked_add(settlement.fee.amount)
        .expect("não passa do consumido");
    let refund = buyer_consumed
        .checked_sub(buyer_paid)
        .expect("limite + taxa por lot cobre valor e taxa de qualquer fill");
    if refund > Amount::ZERO {
        ledger
            .release(buyer.account, spec.quote(), refund)
            .expect("a sobra estava reservada");
    }
    consume(reservations, (instrument, buyer_order), buyer_consumed);
    consume(
        reservations,
        (instrument, seller_order),
        settlement.base.amount,
    );
    fee
}

/// Seção 4.1: tamanho máximo e colar de preço contra o melhor preço do lado oposto. Ordem
/// a mercado sem proteção ganha a borda do colar como proteção implícita.
fn pre_trade_risk(order: Order, book: &OrderBook, risk: RiskLimits) -> Result<Order, RejectReason> {
    if order.total() > risk.max_lots {
        return Err(RejectReason::OrderTooLarge {
            lots: order.total(),
            max: risk.max_lots,
        });
    }
    let side = order.side();
    let reference = match side {
        Side::Bid => book.best_ask(),
        Side::Ask => book.best_bid(),
    };
    match (order.price_bound(), reference) {
        (Some(_), None) => Ok(order),
        (None, None) => Err(RejectReason::NoReferencePrice),
        (None, Some(reference)) => {
            Ok(order.with_implicit_protection(collar_edge(side, reference, risk.collar)))
        }
        (Some(bound), Some(reference)) => {
            let edge = collar_edge(side, reference, risk.collar);
            let outside = match side {
                Side::Bid => bound > edge,
                Side::Ask => bound < edge,
            };
            if outside {
                Err(RejectReason::OutsideCollar { bound, edge })
            } else {
                Ok(order)
            }
        }
    }
}

/// Pior preço que o colar aceita para uma ordem agressiva do lado `side`, dado o melhor
/// preço do lado oposto: `referência ± ⌈referência × colar / 10.000⌉` (mesma regra de
/// arredondamento da taxa). Satura em `i64::MAX` na compra e em 1 na venda.
fn collar_edge(side: Side, reference: Price, collar: Bps) -> Price {
    let band = collar
        .fee_on(Amount::new(reference.get()).expect("preço é positivo"))
        .get();
    let edge = match side {
        Side::Bid => reference.get().saturating_add(band),
        Side::Ask => (reference.get() - band).max(1),
    };
    Price::new(edge).expect("saturado em [1, i64::MAX]")
}

fn reservation_per_lot(
    side: Side,
    bound: Option<Price>,
    instrument: &Instrument,
    taker_bps: Bps,
) -> Result<(AssetId, Amount), RejectReason> {
    match (side, bound) {
        (Side::Bid, Some(bound)) => {
            let price = Amount::new(bound.get()).expect("preço é positivo");
            price
                .checked_add(taker_bps.fee_on(price))
                .map(|per_lot| (instrument.quote(), per_lot))
                .ok_or(RejectReason::ValueOverflow)
        }
        (Side::Bid, None) => unreachable!(
            "o risco pré-trade dá proteção a toda ordem a mercado, e o stop de compra arma com preço"
        ),
        (Side::Ask, _) => Ok((instrument.base(), instrument.lot_size())),
    }
}

/// Trava o que `order` pode gastar, com `bound` como pior preço na compra, e registra a
/// reserva dela. Sem saldo, nada muda.
fn reserve(
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    spec: &Instrument,
    taker_bps: Bps,
    order: &Order,
    bound: Option<Price>,
) -> Result<(), RejectReason> {
    let (asset, per_lot) = reservation_per_lot(order.side(), bound, spec, taker_bps)?;
    let required = per_lot
        .times(order.remaining())
        .ok_or(RejectReason::ValueOverflow)?;
    match ledger.reserve(order.account(), asset, required) {
        Ok(()) => {}
        Err(LedgerError::InsufficientAvailable { available, .. }) => {
            return Err(RejectReason::InsufficientFunds {
                asset,
                available,
                required,
            });
        }
        Err(other) => unreachable!("oferta limitada impede estouro do reservado: {other:?}"),
    }
    reservations.insert(
        (spec.id(), order.id()),
        Reservation {
            account: order.account(),
            asset,
            amount: required,
            per_lot,
        },
    );
    Ok(())
}

/// O pior preço que um stop de compra pode pagar depois de disparado, e com o qual ele
/// reserva ao armar: o limite, ou, num stop a mercado, a borda do colar medida sobre o
/// próprio gatilho. A venda trava lots de base e não precisa de preço.
fn stop_bound(side: Side, trigger: Price, limit: Option<Price>, risk: RiskLimits) -> Option<Price> {
    match side {
        Side::Bid => Some(limit.unwrap_or_else(|| collar_edge(Side::Bid, trigger, risk.collar))),
        Side::Ask => limit,
    }
}

/// Arma um stop: tamanho máximo, gatilho ainda não alcançado e reserva na entrada (o stop
/// nunca falha por saldo no disparo, que é quando o cliente mais conta com ele).
///
/// # Panics
/// Nunca: a ordem acabou de nascer do pedido e só pode estar em `New`.
fn arm(
    listing: &mut Listing,
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    fees: FeeSchedule,
    mut order: Order,
    trigger: Price,
) -> Result<Event, RejectReason> {
    if order.total() > listing.risk.max_lots {
        return Err(RejectReason::OrderTooLarge {
            lots: order.total(),
            max: listing.risk.max_lots,
        });
    }
    let side = order.side();
    if let Some(last) = listing.last_trade {
        let reached = match side {
            Side::Bid => last >= trigger,
            Side::Ask => last <= trigger,
        };
        if reached {
            return Err(RejectReason::StopAlreadyTriggered { trigger, last });
        }
    }
    order
        .transition(OrderStatus::Accepted)
        .expect("ordem recém-construída está em New, e New → Accepted é legal");
    let limit = order.quoted_prices()[1];
    let bound = stop_bound(side, trigger, limit, listing.risk);
    reserve(
        ledger,
        reservations,
        &listing.instrument,
        fees.taker_bps,
        &order,
        bound,
    )?;
    let event = Event::Armed {
        instrument: listing.instrument.id(),
        order: order.id(),
        side,
        trigger,
        lots: order.remaining(),
    };
    listing.stops.arm(order, trigger);
    Ok(event)
}

/// Um stop disparado entra como ordem que acabou de chegar: passa pelo risco pré-trade
/// contra o livro de agora. Recusado, ele é cancelado e a reserva volta inteira; um stop
/// de compra a mercado tem a proteção apertada para caber no que reservou ao armar.
fn fire(
    listing: &mut Listing,
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    fees: FeeSchedule,
    stop: Order,
    trigger: Price,
) -> Vec<Event> {
    let instrument = listing.instrument.id();
    let key = (instrument, stop.id());
    let limit = stop.quoted_prices()[1];
    let lots = stop.remaining();
    let Ok(order) = pre_trade_risk(stop.triggered(), &listing.book, listing.risk) else {
        release_rest(ledger, reservations, key);
        return vec![Event::Cancelled {
            instrument,
            order: key.1,
            lots,
        }];
    };
    let order = match stop_bound(order.side(), trigger, limit, listing.risk) {
        Some(cap) if limit.is_none() => order.with_protection_within(cap),
        _ => order,
    };
    execute(listing, ledger, reservations, fees, order)
}

/// Uma ordem aceita e já reservada contra o livro: liquida cada fill, atualiza o último
/// negócio e fecha a sobra.
///
/// # Panics
/// Nunca por entrada: a reserva cobre todo débito da liquidação e a oferta limitada do
/// ledger impede estouro nos créditos. Se a liquidação falhar, o invariante quebrou e
/// parar é o certo (fail-stop).
fn execute(
    listing: &mut Listing,
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    fees: FeeSchedule,
    order: Order,
) -> Vec<Event> {
    let spec = listing.instrument;
    let instrument = spec.id();
    let taker = Taker {
        id: order.id(),
        side: order.side(),
    };
    let outcome = listing.book.submit(order);
    if let Some(last) = outcome.fills.last() {
        listing.last_trade = Some(last.price);
    }

    let mut events = Vec::new();
    for fill in outcome.fills {
        let fee = settle_fill(ledger, reservations, &spec, fees, taker, &fill);
        events.push(Event::Fill {
            instrument,
            fill,
            fee,
        });
    }
    events.extend(close_leftover(
        ledger,
        reservations,
        (instrument, taker.id),
        taker.side,
        outcome.leftover,
    ));
    events
}

fn fill_settlement(
    instrument: &Instrument,
    fill: &Fill,
    parties: Parties,
    fee: Amount,
    taker_side: Side,
) -> FillSettlement {
    let base = instrument
        .lot_size()
        .times(fill.lots)
        .expect("já coube na reserva do vendedor, calculada sem estourar");
    let value = fill
        .price
        .times(fill.lots)
        .expect("não passa da reserva do comprador, calculada sem estourar");
    let to_seller = match taker_side {
        Side::Bid => value,
        Side::Ask => value
            .checked_sub(fee)
            .expect("taxa nunca passa do valor (bps <= 100%)"),
    };
    FillSettlement {
        base: Transfer {
            asset: instrument.base(),
            from: parties.seller,
            to: parties.buyer,
            amount: base,
        },
        quote: Transfer {
            asset: instrument.quote(),
            from: parties.buyer,
            to: parties.seller,
            amount: to_seller,
        },
        fee: Transfer {
            asset: instrument.quote(),
            from: parties.buyer,
            to: parties.house,
            amount: fee,
        },
    }
}

/// O evento da sobra do taker e, quando ela não descansa, a devolução do que ainda
/// estava reservado para ela.
fn close_leftover(
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    key: (InstrumentId, OrderId),
    side: Side,
    leftover: Leftover,
) -> Option<Event> {
    let (instrument, order) = key;
    match leftover {
        Leftover::None => None,
        Leftover::Rested { price, lots } => Some(Event::Rested {
            instrument,
            order,
            side,
            price,
            lots,
        }),
        Leftover::Cancelled(lots) => {
            release_rest(ledger, reservations, key);
            Some(Event::Cancelled {
                instrument,
                order,
                lots,
            })
        }
        Leftover::Expired(lots) => {
            release_rest(ledger, reservations, key);
            Some(Event::Expired {
                instrument,
                order,
                lots,
            })
        }
    }
}

fn consume(reservations: &mut Reservations, key: (InstrumentId, OrderId), amount: Amount) {
    let reservation = reservations
        .get_mut(&key)
        .expect("ordem com fill tem reserva");
    reservation.amount = reservation
        .amount
        .checked_sub(amount)
        .expect("consumo nunca passa do reservado");
    if reservation.amount == Amount::ZERO {
        reservations.remove(&key);
    }
}

fn release_rest(
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    key: (InstrumentId, OrderId),
) {
    if let Some(reservation) = reservations.remove(&key) {
        ledger
            .release(reservation.account, reservation.asset, reservation.amount)
            .expect("o que a ordem travou continua reservado");
    }
}

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{OrderKind, Seq, TimeInForce};

    const TESTE_BRL: InstrumentId = InstrumentId::new(1);
    const OUTRO_BRL: InstrumentId = InstrumentId::new(2);
    const BRL: AssetId = AssetId::new(1);
    const TESTE: AssetId = AssetId::new(10);
    const ALICE: AccountId = AccountId::new(1);
    const BOB: AccountId = AccountId::new(2);
    const PLENTY: i64 = 1_000_000_000;
    const HOUSE: AccountId = AccountId::new(99);

    fn limits(max_lots: i64, collar_bps: u32) -> RiskLimits {
        RiskLimits {
            max_lots: lots(max_lots),
            collar: Bps::new(collar_bps).expect("bps de teste válido"),
        }
    }

    // Colar de 100% e tamanho sem limite prático: para os testes que não são de risco.
    fn wide(instrument: Instrument) -> ListingConfig {
        ListingConfig {
            instrument,
            risk: limits(i64::MAX, 10_000),
        }
    }

    fn fees(taker_bps: u32) -> FeeSchedule {
        FeeSchedule {
            house: HOUSE,
            taker_bps: Bps::new(taker_bps).expect("bps de teste válido"),
        }
    }

    fn amount(value: i64) -> Amount {
        Amount::new(value).expect("quantia de teste válida")
    }

    fn spec_with_lot(id: InstrumentId, tick: i64, lot_size: i64) -> Instrument {
        Instrument::new(
            id,
            AssetId::new(id.get() * 10),
            BRL,
            Price::new(tick).expect("tick de teste válido"),
            amount(lot_size),
        )
        .expect("instrumento de teste válido")
    }

    fn spec(id: InstrumentId, tick: i64) -> Instrument {
        spec_with_lot(id, tick, 1)
    }

    fn deposit(engine: &mut Engine, account: AccountId, asset: AssetId, value: i64) {
        let events = engine.apply(Command::Deposit {
            account,
            asset,
            amount: amount(value),
        });
        assert!(matches!(events.as_slice(), [Event::Deposited { .. }]));
    }

    // Alice e Bob com saldo de sobra em todo ativo dos instrumentos, para os testes de
    // matching não esbarrarem em reserva.
    fn engine_with(specs: &[Instrument]) -> Engine {
        let mut engine =
            Engine::new(specs.iter().copied().map(wide), fees(0)).expect("ids de teste distintos");
        for spec in specs {
            for asset in [spec.base(), spec.quote()] {
                for account in [ALICE, BOB] {
                    deposit(&mut engine, account, asset, PLENTY);
                }
            }
        }
        engine
    }

    fn engine(ids: &[InstrumentId]) -> Engine {
        engine_with(&ids.iter().map(|id| spec(*id, 1)).collect::<Vec<_>>())
    }

    fn balance(available: i64, reserved: i64) -> Balance {
        Balance {
            available: amount(available),
            reserved: amount(reserved),
        }
    }

    fn request(
        account: AccountId,
        id: u64,
        instrument: InstrumentId,
        side: Side,
        kind: OrderKind,
        quantity: i64,
    ) -> OrderRequest {
        OrderRequest {
            id: OrderId::new(id),
            account,
            instrument,
            side,
            kind,
            total: lots(quantity),
            seq: Seq::new(id),
            time_in_force: TimeInForce::Gtc,
        }
    }

    fn order_for(
        account: AccountId,
        id: u64,
        side: Side,
        price: i64,
        quantity: i64,
    ) -> OrderRequest {
        let kind = OrderKind::Limit(Price::new(price).expect("preço de teste válido"));
        request(account, id, TESTE_BRL, side, kind, quantity)
    }

    fn market_for(account: AccountId, id: u64, side: Side, quantity: i64) -> OrderRequest {
        protected_for(account, id, side, quantity, None)
    }

    fn protected_for(
        account: AccountId,
        id: u64,
        side: Side,
        quantity: i64,
        protection: Option<i64>,
    ) -> OrderRequest {
        let protection =
            protection.map(|bound| Price::new(bound).expect("proteção de teste válida"));
        request(
            account,
            id,
            TESTE_BRL,
            side,
            OrderKind::Market { protection },
            quantity,
        )
    }

    fn submit(engine: &mut Engine, order: OrderRequest) -> Vec<Event> {
        engine.apply(Command::Submit(order))
    }

    // Engine "pobre": Alice tem 1000 BRL, Bob tem 10 lots de TESTE (lot de 100 unidades).
    fn lean_engine() -> Engine {
        lean_engine_with_fee(0)
    }

    fn lean_engine_with_fee(taker_bps: u32) -> Engine {
        let mut engine = Engine::new([wide(spec_with_lot(TESTE_BRL, 1, 100))], fees(taker_bps))
            .expect("config válida");
        deposit(&mut engine, ALICE, BRL, 1_000);
        deposit(&mut engine, BOB, TESTE, 1_000);
        engine
    }

    // Seção 4.2: B leva 2 lots @ 100 de S com taxa de taker t para H.
    // Com 50 bps, t = ⌈200 × 50 / 10.000⌉ = 1.
    #[test]
    fn section_4_2_example_with_taker_fee() {
        let mut engine = lean_engine_with_fee(50);
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 2));

        let events = submit(&mut engine, order_for(ALICE, 2, Side::Bid, 100, 2));

        assert!(matches!(
            events.as_slice(),
            [Event::Fill { fee, .. }] if *fee == amount(1)
        ));
        assert_eq!(engine.balance(ALICE, BRL), balance(799, 0));
        assert_eq!(engine.balance(ALICE, TESTE), balance(200, 0));
        assert_eq!(engine.balance(BOB, BRL), balance(200, 0));
        assert_eq!(engine.balance(HOUSE, BRL), balance(1, 0));
    }

    #[test]
    fn buy_reserves_the_fee_of_one_lot_per_lot() {
        let mut engine = lean_engine_with_fee(50);

        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 3));

        assert_eq!(engine.balance(ALICE, BRL), balance(697, 303));
    }

    #[test]
    fn taker_seller_pays_the_fee_out_of_the_proceeds() {
        let mut engine = lean_engine_with_fee(50);
        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 2));

        submit(&mut engine, order_for(BOB, 2, Side::Ask, 100, 2));

        assert_eq!(engine.balance(BOB, BRL), balance(199, 0));
        assert_eq!(engine.balance(HOUSE, BRL), balance(1, 0));
        assert_eq!(engine.balance(ALICE, BRL), balance(800, 0));
    }

    #[test]
    fn maker_buyer_pays_no_fee_and_gets_the_fee_reserve_back() {
        let mut engine = lean_engine_with_fee(50);
        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 3));

        submit(&mut engine, order_for(BOB, 2, Side::Ask, 100, 1));

        assert_eq!(engine.balance(ALICE, BRL), balance(698, 202));
    }

    // ⌈a⌉ + ⌈b⌉ ≥ ⌈a + b⌉: três fills de 1 lot pagam 3 de taxa, embora a taxa do total
    // seja 1. A reserva por lot cobre exatamente isso.
    #[test]
    fn per_fill_rounding_is_covered_by_the_per_lot_reserve() {
        let mut engine = lean_engine_with_fee(10);
        for id in 1..=3 {
            submit(&mut engine, order_for(BOB, id, Side::Ask, 1, 1));
        }

        let events = submit(&mut engine, order_for(ALICE, 4, Side::Bid, 1, 3));

        assert_eq!(events.len(), 3);
        assert_eq!(engine.balance(HOUSE, BRL), balance(3, 0));
        assert_eq!(engine.balance(ALICE, BRL), balance(994, 0));
    }

    #[test]
    fn house_account_cannot_trade() {
        let mut engine = lean_engine_with_fee(50);

        let events = submit(&mut engine, order_for(HOUSE, 1, Side::Bid, 100, 1));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::HouseCannotTrade,
            }]
        );
    }

    #[test]
    fn deposit_credits_available() {
        let mut engine = Engine::new([wide(spec(TESTE_BRL, 1))], fees(0)).expect("config válida");

        let events = engine.apply(Command::Deposit {
            account: ALICE,
            asset: BRL,
            amount: amount(500),
        });

        assert_eq!(
            events,
            vec![Event::Deposited {
                account: ALICE,
                asset: BRL,
                amount: amount(500),
            }]
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(500, 0));
    }

    #[test]
    fn deposit_beyond_supply_is_rejected() {
        let mut engine = Engine::new([wide(spec(TESTE_BRL, 1))], fees(0)).expect("config válida");
        deposit(&mut engine, ALICE, BRL, i64::MAX);

        let events = engine.apply(Command::Deposit {
            account: BOB,
            asset: BRL,
            amount: amount(1),
        });

        assert!(matches!(events.as_slice(), [Event::DepositRejected { .. }]));
        assert_eq!(engine.balance(BOB, BRL), balance(0, 0));
    }

    #[test]
    fn buy_without_funds_is_rejected_and_never_reaches_the_book() {
        let mut engine = lean_engine();

        let events = submit(&mut engine, order_for(ALICE, 1, Side::Bid, 300, 4));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::InsufficientFunds {
                    asset: BRL,
                    available: amount(1_000),
                    required: amount(1_200),
                },
            }]
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(1_000, 0));
        assert_eq!(engine.book(TESTE_BRL).unwrap().best_bid(), None);
    }

    #[test]
    fn resting_buy_reserves_price_times_lots() {
        let mut engine = lean_engine();

        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 3));

        assert_eq!(engine.balance(ALICE, BRL), balance(700, 300));
    }

    #[test]
    fn resting_sell_reserves_lots_times_lot_size() {
        let mut engine = lean_engine();

        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 3));

        assert_eq!(engine.balance(BOB, TESTE), balance(700, 300));
    }

    #[test]
    fn trade_moves_base_and_quote_between_buyer_and_seller() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 3));

        submit(&mut engine, order_for(ALICE, 2, Side::Bid, 100, 2));

        assert_eq!(engine.balance(ALICE, BRL), balance(800, 0));
        assert_eq!(engine.balance(ALICE, TESTE), balance(200, 0));
        assert_eq!(engine.balance(BOB, BRL), balance(200, 0));
        assert_eq!(engine.balance(BOB, TESTE), balance(700, 100));
    }

    #[test]
    fn taker_buyer_gets_price_improvement_back() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 90, 2));

        submit(&mut engine, order_for(ALICE, 2, Side::Bid, 100, 2));

        assert_eq!(engine.balance(ALICE, BRL), balance(820, 0));
        assert_eq!(engine.balance(BOB, BRL), balance(180, 0));
    }

    #[test]
    fn partially_filled_buyer_keeps_exactly_the_remainder_reserved() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 90, 1));

        submit(&mut engine, order_for(ALICE, 2, Side::Bid, 100, 3));

        assert_eq!(engine.balance(ALICE, BRL), balance(710, 200));
    }

    #[test]
    fn cancel_releases_exactly_what_was_still_reserved() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 3));
        submit(&mut engine, order_for(BOB, 2, Side::Ask, 100, 1));

        engine.apply(Command::Cancel {
            instrument: TESTE_BRL,
            account: ALICE,
            order: OrderId::new(1),
        });

        assert_eq!(engine.balance(ALICE, BRL), balance(900, 0));
    }

    #[test]
    fn ioc_remainder_and_expired_fok_release_their_reserve() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 1));

        submit(
            &mut engine,
            OrderRequest {
                time_in_force: TimeInForce::Ioc,
                ..order_for(ALICE, 2, Side::Bid, 100, 3)
            },
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(900, 0));

        submit(
            &mut engine,
            OrderRequest {
                time_in_force: TimeInForce::Fok,
                ..order_for(ALICE, 3, Side::Bid, 100, 5)
            },
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(900, 0));
    }

    #[test]
    fn market_order_without_reference_price_is_rejected() {
        let mut engine = lean_engine();

        let events = submit(&mut engine, market_for(ALICE, 1, Side::Bid, 1));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::NoReferencePrice,
            }]
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(1_000, 0));
    }

    fn price(value: i64) -> Price {
        Price::new(value).expect("preço de teste válido")
    }

    fn bps(value: u32) -> Bps {
        Bps::new(value).expect("bps de teste válido")
    }

    #[test]
    fn collar_edge_is_reference_plus_or_minus_the_band() {
        assert_eq!(collar_edge(Side::Bid, price(100), bps(1_000)), price(110));
        assert_eq!(collar_edge(Side::Ask, price(100), bps(1_000)), price(90));
    }

    #[test]
    fn collar_band_rounds_up_like_the_fee() {
        assert_eq!(collar_edge(Side::Bid, price(101), bps(1_000)), price(112));
        assert_eq!(collar_edge(Side::Ask, price(101), bps(1_000)), price(90));
    }

    #[test]
    fn zero_collar_edge_is_the_reference_itself() {
        assert_eq!(collar_edge(Side::Bid, price(100), bps(0)), price(100));
        assert_eq!(collar_edge(Side::Ask, price(100), bps(0)), price(100));
    }

    #[test]
    fn collar_edge_saturates_instead_of_overflowing_or_hitting_zero() {
        assert_eq!(
            collar_edge(Side::Bid, price(i64::MAX), bps(1)),
            price(i64::MAX)
        );
        assert_eq!(collar_edge(Side::Ask, price(5), bps(10_000)), price(1));
    }

    // Instrumento com lot 100, colar de 10% e no máximo 5 lots por ordem.
    fn guarded_engine() -> Engine {
        let config = ListingConfig {
            instrument: spec_with_lot(TESTE_BRL, 1, 100),
            risk: limits(5, 1_000),
        };
        let mut engine = Engine::new([config], fees(0)).expect("config válida");
        deposit(&mut engine, ALICE, BRL, PLENTY);
        deposit(&mut engine, BOB, TESTE, PLENTY);
        engine
    }

    #[test]
    fn order_larger_than_the_limit_is_rejected_before_reserving() {
        let mut engine = guarded_engine();

        let events = submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 6));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::OrderTooLarge {
                    lots: lots(6),
                    max: lots(5),
                },
            }]
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(PLENTY, 0));
    }

    #[test]
    fn fat_finger_buy_above_the_collar_is_rejected() {
        let mut engine = guarded_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 5));

        let events = submit(&mut engine, order_for(ALICE, 2, Side::Bid, 1_000, 1));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(2),
                reason: RejectReason::OutsideCollar {
                    bound: price(1_000),
                    edge: price(110),
                },
            }]
        );
    }

    #[test]
    fn fat_finger_sell_below_the_collar_is_rejected() {
        let mut engine = guarded_engine();
        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 5));

        let events = submit(&mut engine, order_for(BOB, 2, Side::Ask, 10, 1));

        assert!(matches!(
            events.as_slice(),
            [Event::Rejected {
                reason: RejectReason::OutsideCollar { .. },
                ..
            }]
        ));
    }

    #[test]
    fn passive_order_far_from_the_market_is_fine() {
        let mut engine = guarded_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 5));

        let events = submit(&mut engine, order_for(ALICE, 2, Side::Bid, 1, 1));

        assert!(matches!(events.as_slice(), [Event::Rested { .. }]));
    }

    #[test]
    fn explicit_protection_outside_the_collar_is_rejected() {
        let mut engine = guarded_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 5));

        let events = submit(
            &mut engine,
            protected_for(ALICE, 2, Side::Bid, 1, Some(500)),
        );

        assert!(matches!(
            events.as_slice(),
            [Event::Rejected {
                reason: RejectReason::OutsideCollar { .. },
                ..
            }]
        ));
    }

    #[test]
    fn plain_market_buy_gets_the_collar_as_protection_and_stops_there() {
        let mut engine = guarded_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 2));
        submit(&mut engine, order_for(BOB, 2, Side::Ask, 200, 2));

        let events = submit(&mut engine, market_for(ALICE, 3, Side::Bid, 4));

        assert!(matches!(
            events.as_slice(),
            [Event::Fill { .. }, Event::Cancelled { lots: remainder, .. }] if *remainder == lots(2)
        ));
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_ask(), Some(price(200)));
        assert_eq!(engine.balance(ALICE, BRL).reserved, Amount::ZERO);
    }

    #[test]
    fn protected_market_buy_reserves_its_bound_and_returns_the_rest() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 90, 2));
        submit(&mut engine, order_for(BOB, 2, Side::Ask, 120, 2));

        let events = submit(
            &mut engine,
            protected_for(ALICE, 3, Side::Bid, 4, Some(100)),
        );

        assert!(matches!(
            events.as_slice(),
            [Event::Fill { .. }, Event::Cancelled { lots: remainder, .. }] if *remainder == lots(2)
        ));
        assert_eq!(engine.balance(ALICE, BRL), balance(820, 0));
        assert_eq!(engine.balance(ALICE, TESTE), balance(200, 0));
    }

    #[test]
    fn protected_market_buy_without_funds_for_its_bound_is_rejected() {
        let mut engine = lean_engine();

        let events = submit(
            &mut engine,
            protected_for(ALICE, 1, Side::Bid, 4, Some(300)),
        );

        assert!(matches!(
            events.as_slice(),
            [Event::Rejected {
                reason: RejectReason::InsufficientFunds { .. },
                ..
            }]
        ));
    }

    #[test]
    fn market_sell_reserves_base_and_releases_unfilled_part() {
        let mut engine = lean_engine();
        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 2));

        submit(&mut engine, market_for(BOB, 2, Side::Ask, 5));

        assert_eq!(engine.balance(BOB, TESTE), balance(800, 0));
        assert_eq!(engine.balance(BOB, BRL), balance(200, 0));
        assert_eq!(engine.balance(ALICE, TESTE), balance(200, 0));
    }

    #[test]
    fn duplicate_instrument_is_a_config_error() {
        let result = Engine::new(
            [wide(spec(TESTE_BRL, 1)), wide(spec(TESTE_BRL, 5))],
            fees(0),
        );

        assert_eq!(
            result.err(),
            Some(ConfigError::DuplicateInstrument(TESTE_BRL))
        );
    }

    #[test]
    fn engine_knows_the_instrument_specification() {
        let engine = engine_with(&[spec(TESTE_BRL, 10)]);

        let instrument = engine
            .instrument(TESTE_BRL)
            .expect("instrumento registrado");
        assert_eq!(
            instrument.tick(),
            Price::new(10).expect("tick de teste válido")
        );
    }

    #[test]
    fn limit_price_off_tick_is_rejected_before_the_book() {
        let mut engine = engine_with(&[spec(TESTE_BRL, 10)]);

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Bid, 105, 1)));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::OffTick {
                    price: Price::new(105).expect("preço de teste válido"),
                    tick: Price::new(10).expect("tick de teste válido"),
                },
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn limit_price_on_tick_is_accepted() {
        let mut engine = engine_with(&[spec(TESTE_BRL, 10)]);

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Bid, 110, 1)));

        assert!(matches!(events.as_slice(), [Event::Rested { .. }]));
    }

    #[test]
    fn market_order_has_no_price_to_check() {
        let mut engine = engine_with(&[spec(TESTE_BRL, 10)]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Bid, 110, 1)));
        let market = market_for(BOB, 2, Side::Ask, 1);

        let events = engine.apply(Command::Submit(market));

        assert!(matches!(events.as_slice(), [Event::Fill { .. }]));
    }

    fn order(
        id: u64,
        instrument: InstrumentId,
        side: Side,
        price: i64,
        quantity: i64,
    ) -> OrderRequest {
        let kind = OrderKind::Limit(Price::new(price).expect("preço de teste válido"));
        request(AccountId::new(1), id, instrument, side, kind, quantity)
    }

    fn lots(value: i64) -> Lots {
        Lots::new(value).expect("lots de teste válidos")
    }

    #[test]
    fn unknown_instrument_is_rejected() {
        let mut engine = engine(&[TESTE_BRL]);
        let unknown = InstrumentId::new(99);

        let events = engine.apply(Command::Submit(order(1, unknown, Side::Bid, 100, 5)));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::UnknownInstrument(unknown),
            }]
        );
    }

    #[test]
    fn empty_order_is_rejected_before_the_book() {
        let mut engine = engine(&[TESTE_BRL]);

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Bid, 100, 0)));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::EmptyOrder,
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_bid(), None);
    }

    fn cancel(instrument: InstrumentId, account: u64, order: u64) -> Command {
        Command::Cancel {
            instrument,
            account: AccountId::new(account),
            order: OrderId::new(order),
        }
    }

    #[test]
    fn cancel_resting_order_emits_cancelled_with_remainder() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));
        engine.apply(Command::Submit(order(2, TESTE_BRL, Side::Bid, 100, 2)));

        let events = engine.apply(cancel(TESTE_BRL, 1, 1));

        assert_eq!(
            events,
            vec![Event::Cancelled {
                instrument: TESTE_BRL,
                order: OrderId::new(1),
                lots: lots(3),
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn cancel_in_unknown_instrument_is_rejected() {
        let mut engine = engine(&[TESTE_BRL]);
        let unknown = InstrumentId::new(99);

        let events = engine.apply(cancel(unknown, 1, 1));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::UnknownInstrument(unknown),
            }]
        );
    }

    #[test]
    fn cancel_of_unknown_order_is_rejected() {
        let mut engine = engine(&[TESTE_BRL]);

        let events = engine.apply(cancel(TESTE_BRL, 1, 7));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(7),
                reason: RejectReason::UnknownOrder,
            }]
        );
    }

    #[test]
    fn cancel_by_another_account_is_rejected_and_order_stays() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));

        let events = engine.apply(cancel(TESTE_BRL, 2, 1));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::NotOwner,
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_ask(), Price::new(100).ok());
    }

    #[test]
    fn duplicate_order_id_is_rejected_and_original_stays() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 200, 1)));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::DuplicateOrderId,
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_ask(), Price::new(100).ok());
    }

    #[test]
    fn ioc_remainder_emits_cancelled_after_fills() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 2)));
        let ioc = OrderRequest {
            time_in_force: TimeInForce::Ioc,
            ..order(2, TESTE_BRL, Side::Bid, 100, 5)
        };

        let events = engine.apply(Command::Submit(ioc));

        assert!(matches!(
            events.as_slice(),
            [
                Event::Fill { .. },
                Event::Cancelled {
                    order,
                    lots: remainder,
                    ..
                },
            ] if *order == OrderId::new(2) && *remainder == lots(3)
        ));
    }

    #[test]
    fn fok_without_liquidity_emits_only_expired() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 2)));
        let fok = OrderRequest {
            time_in_force: TimeInForce::Fok,
            ..order(2, TESTE_BRL, Side::Bid, 100, 5)
        };

        let events = engine.apply(Command::Submit(fok));

        assert_eq!(
            events,
            vec![Event::Expired {
                instrument: TESTE_BRL,
                order: OrderId::new(2),
                lots: lots(5),
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_ask(), Price::new(100).ok());
    }

    #[test]
    fn order_without_counterparty_rests() {
        let mut engine = engine(&[TESTE_BRL]);

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Bid, 100, 5)));

        assert_eq!(
            events,
            vec![Event::Rested {
                instrument: TESTE_BRL,
                order: OrderId::new(1),
                side: Side::Bid,
                price: Price::new(100).expect("preço de teste válido"),
                lots: lots(5),
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_bid(), Price::new(100).ok());
    }

    #[test]
    fn rested_sell_carries_its_side_and_limit_price() {
        let mut engine = engine(&[TESTE_BRL]);

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 120, 4)));

        assert_eq!(
            events,
            vec![Event::Rested {
                instrument: TESTE_BRL,
                order: OrderId::new(1),
                side: Side::Ask,
                price: Price::new(120).expect("preço de teste válido"),
                lots: lots(4),
            }]
        );
    }

    #[test]
    fn fills_come_before_rested_remainder() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 2)));

        let events = engine.apply(Command::Submit(order(2, TESTE_BRL, Side::Bid, 100, 5)));

        assert_eq!(
            events,
            vec![
                Event::Fill {
                    instrument: TESTE_BRL,
                    fill: Fill {
                        taker: OrderId::new(2),
                        maker: OrderId::new(1),
                        price: Price::new(100).expect("preço de teste válido"),
                        lots: lots(2),
                    },
                    fee: Amount::ZERO,
                },
                Event::Rested {
                    instrument: TESTE_BRL,
                    order: OrderId::new(2),
                    side: Side::Bid,
                    price: Price::new(100).expect("preço de teste válido"),
                    lots: lots(3),
                },
            ]
        );
    }

    #[test]
    fn fully_filled_order_does_not_rest() {
        let mut engine = engine(&[TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));

        let events = engine.apply(Command::Submit(order(2, TESTE_BRL, Side::Bid, 100, 5)));

        assert!(matches!(events.as_slice(), [Event::Fill { .. }]));
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn instruments_do_not_match_each_other() {
        let mut engine = engine(&[TESTE_BRL, OUTRO_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));

        let events = engine.apply(Command::Submit(order(2, OUTRO_BRL, Side::Bid, 100, 5)));

        assert!(matches!(events.as_slice(), [Event::Rested { .. }]));
        let teste = engine.book(TESTE_BRL).expect("instrumento registrado");
        let outro = engine.book(OUTRO_BRL).expect("instrumento registrado");
        assert_eq!(teste.best_ask(), Price::new(100).ok());
        assert_eq!(outro.best_bid(), Price::new(100).ok());
    }

    fn stop(
        account: AccountId,
        id: u64,
        side: Side,
        trigger: i64,
        limit: Option<i64>,
        quantity: i64,
    ) -> OrderRequest {
        let kind = OrderKind::Stop {
            trigger: price(trigger),
            limit: limit.map(price),
        };
        request(account, id, TESTE_BRL, side, kind, quantity)
    }

    /// Os eventos num formato curto, para comparar a sequência inteira de uma vez.
    fn trace(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                Event::Fill { fill, .. } => format!(
                    "fill #{} @{} x{}",
                    fill.taker.get(),
                    fill.price.get(),
                    fill.lots.get()
                ),
                Event::Armed { order, .. } => format!("armed #{}", order.get()),
                Event::Triggered { order, .. } => format!("triggered #{}", order.get()),
                Event::Rested {
                    order, price, lots, ..
                } => format!("rested #{} @{} x{}", order.get(), price.get(), lots.get()),
                Event::Cancelled { order, lots, .. } => {
                    format!("cancelled #{} x{}", order.get(), lots.get())
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    // Stop de compra a mercado reserva na entrada a borda do colar sobre o próprio
    // gatilho: 105 + ⌈105 × 10%⌉ = 116 por lot, e fica fora do livro.
    #[test]
    fn stop_arms_with_its_reserve_and_stays_out_of_the_book() {
        let mut engine = guarded_engine();

        let events = submit(&mut engine, stop(ALICE, 1, Side::Bid, 105, None, 2));

        assert_eq!(trace(&events), ["armed #1"]);
        assert_eq!(engine.balance(ALICE, BRL).reserved, amount(232));
        assert_eq!(engine.book(TESTE_BRL).unwrap().best_bid(), None);
    }

    #[test]
    fn stop_waits_for_a_trade_and_fires_after_the_order_that_reached_it() {
        let mut engine = engine(&[TESTE_BRL]);
        for (id, ask) in [(1, 100), (2, 105)] {
            submit(&mut engine, order_for(BOB, id, Side::Ask, ask, 1));
        }
        submit(&mut engine, order_for(BOB, 3, Side::Ask, 110, 5));
        assert_eq!(
            trace(&submit(
                &mut engine,
                stop(ALICE, 4, Side::Bid, 105, None, 2)
            )),
            ["armed #4"]
        );

        let events = submit(&mut engine, order_for(ALICE, 5, Side::Bid, 105, 2));

        assert_eq!(
            trace(&events),
            [
                "fill #5 @100 x1",
                "fill #5 @105 x1",
                "triggered #4",
                "fill #4 @110 x2"
            ]
        );
    }

    #[test]
    fn fired_stop_limit_rests_its_remainder_at_the_limit() {
        let mut engine = engine(&[TESTE_BRL]);
        for (id, bid) in [(1, 96), (2, 95), (3, 94)] {
            submit(&mut engine, order_for(ALICE, id, Side::Bid, bid, 1));
        }
        submit(&mut engine, stop(BOB, 4, Side::Ask, 95, Some(94), 3));

        let events = submit(&mut engine, order_for(BOB, 5, Side::Ask, 95, 2));

        assert_eq!(
            trace(&events),
            [
                "fill #5 @96 x1",
                "fill #5 @95 x1",
                "triggered #4",
                "fill #4 @94 x1",
                "rested #4 @94 x2"
            ]
        );
    }

    #[test]
    fn stop_already_reached_by_the_last_trade_is_rejected() {
        let mut engine = engine(&[TESTE_BRL]);
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 1));
        submit(&mut engine, order_for(ALICE, 2, Side::Bid, 100, 1));
        let reached = RejectReason::StopAlreadyTriggered {
            trigger: price(100),
            last: price(100),
        };

        assert_eq!(
            submit(&mut engine, stop(ALICE, 3, Side::Bid, 100, None, 1)),
            [Event::Rejected {
                order: OrderId::new(3),
                reason: reached
            }]
        );
        assert_eq!(
            submit(&mut engine, stop(ALICE, 4, Side::Ask, 100, None, 1)),
            [Event::Rejected {
                order: OrderId::new(4),
                reason: reached
            }]
        );
        assert_eq!(
            trace(&submit(
                &mut engine,
                stop(ALICE, 5, Side::Bid, 101, None, 1)
            )),
            ["armed #5"]
        );
        assert_eq!(
            trace(&submit(&mut engine, stop(ALICE, 6, Side::Ask, 99, None, 1))),
            ["armed #6"]
        );
    }

    #[test]
    fn cancelling_a_dormant_stop_releases_its_reserve_and_checks_the_owner() {
        let mut engine = guarded_engine();
        submit(&mut engine, stop(ALICE, 1, Side::Bid, 105, None, 2));
        let cancel = |account| Command::Cancel {
            instrument: TESTE_BRL,
            account,
            order: OrderId::new(1),
        };

        assert_eq!(
            engine.apply(cancel(BOB)),
            [Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::NotOwner
            }]
        );
        assert_eq!(trace(&engine.apply(cancel(ALICE))), ["cancelled #1 x2"]);
        assert_eq!(engine.balance(ALICE, BRL), balance(PLENTY, 0));
    }

    // Os stops de venda em 97 (chegou antes) e em 99: o negócio em 99 dispara o de 99,
    // cuja venda negocia em 97 e dispara o de 97 — cascata, do gatilho mais perto ao mais
    // longe, dentro do mesmo comando.
    #[test]
    fn cascade_fires_stops_from_the_closest_trigger_out() {
        let mut engine = engine(&[TESTE_BRL]);
        submit(&mut engine, order_for(ALICE, 1, Side::Bid, 99, 1));
        submit(&mut engine, order_for(ALICE, 2, Side::Bid, 97, 1));
        submit(&mut engine, order_for(ALICE, 3, Side::Bid, 95, 5));
        submit(&mut engine, stop(BOB, 4, Side::Ask, 97, None, 1));
        submit(&mut engine, stop(BOB, 5, Side::Ask, 99, None, 1));

        let events = submit(&mut engine, order_for(BOB, 6, Side::Ask, 99, 1));

        assert_eq!(
            trace(&events),
            [
                "fill #6 @99 x1",
                "triggered #5",
                "fill #5 @97 x1",
                "triggered #4",
                "fill #4 @95 x1"
            ]
        );
    }

    #[test]
    fn stops_with_the_same_trigger_fire_in_arrival_order() {
        let mut engine = engine(&[TESTE_BRL]);
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 1));
        submit(&mut engine, order_for(BOB, 2, Side::Ask, 101, 5));
        submit(&mut engine, stop(ALICE, 3, Side::Bid, 100, None, 1));
        submit(&mut engine, stop(BOB, 4, Side::Bid, 100, None, 1));

        let events = submit(&mut engine, order_for(ALICE, 5, Side::Bid, 100, 1));

        assert_eq!(
            trace(&events),
            [
                "fill #5 @100 x1",
                "triggered #3",
                "fill #3 @101 x1",
                "triggered #4",
                "fill #4 @101 x1"
            ]
        );
    }

    // Disparado, o stop é uma ordem chegando agora: sem ninguém do outro lado, a ordem a
    // mercado não tem referência, e o stop é cancelado com a reserva devolvida.
    #[test]
    fn fired_stop_that_fails_pre_trade_risk_is_cancelled_and_released() {
        let mut engine = engine(&[TESTE_BRL]);
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 105, 1));
        submit(&mut engine, stop(ALICE, 2, Side::Bid, 105, None, 2));

        let events = submit(&mut engine, order_for(ALICE, 3, Side::Bid, 105, 1));

        assert_eq!(
            trace(&events),
            ["fill #3 @105 x1", "triggered #2", "cancelled #2 x2"]
        );
        assert_eq!(engine.balance(ALICE, BRL).reserved, Amount::ZERO);
    }

    // Reservou a 110 (gatilho 100 + 10%); no disparo o colar do livro iria até 134, mas a
    // proteção fica em 110 e a oferta de 121 não é alcançada: sem isso, a liquidação
    // passaria da reserva.
    #[test]
    fn fired_buy_stop_never_pays_above_what_it_reserved() {
        let mut engine = guarded_engine();
        submit(&mut engine, order_for(BOB, 1, Side::Ask, 100, 1));
        submit(&mut engine, order_for(BOB, 2, Side::Ask, 121, 1));
        submit(&mut engine, stop(ALICE, 3, Side::Bid, 100, None, 1));

        let events = submit(&mut engine, order_for(ALICE, 4, Side::Bid, 100, 1));

        assert_eq!(
            trace(&events),
            ["fill #4 @100 x1", "triggered #3", "cancelled #3 x1"]
        );
        assert_eq!(engine.balance(ALICE, BRL).reserved, Amount::ZERO);
    }

    #[test]
    fn stop_prices_must_be_on_the_tick() {
        let mut engine = engine_with(&[spec(TESTE_BRL, 5)]);
        let off_tick = |value| RejectReason::OffTick {
            price: price(value),
            tick: price(5),
        };

        assert_eq!(
            submit(&mut engine, stop(ALICE, 1, Side::Bid, 103, None, 1)),
            [Event::Rejected {
                order: OrderId::new(1),
                reason: off_tick(103)
            }]
        );
        assert_eq!(
            submit(&mut engine, stop(ALICE, 2, Side::Bid, 105, Some(108), 1)),
            [Event::Rejected {
                order: OrderId::new(2),
                reason: off_tick(108)
            }]
        );
    }

    #[test]
    fn id_of_a_dormant_stop_cannot_be_reused() {
        let mut engine = engine(&[TESTE_BRL]);
        submit(&mut engine, stop(ALICE, 1, Side::Bid, 105, None, 1));

        assert_eq!(
            submit(&mut engine, order_for(ALICE, 1, Side::Bid, 100, 1)),
            [Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::DuplicateOrderId
            }]
        );
    }

    #[test]
    fn trades_in_one_instrument_do_not_fire_stops_of_another() {
        let mut engine = engine(&[TESTE_BRL, OUTRO_BRL]);
        let stop_kind = OrderKind::Stop {
            trigger: price(100),
            limit: None,
        };
        submit(
            &mut engine,
            request(ALICE, 1, OUTRO_BRL, Side::Bid, stop_kind, 1),
        );
        submit(&mut engine, order_for(BOB, 2, Side::Ask, 100, 1));

        let events = submit(&mut engine, order_for(ALICE, 3, Side::Bid, 100, 1));

        assert_eq!(trace(&events), ["fill #3 @100 x1"]);
    }
}
