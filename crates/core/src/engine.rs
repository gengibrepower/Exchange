use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::book::{CancelError, Leftover, OrderBook};
use crate::domain::{
    AccountId, Amount, AssetId, Bps, Fill, Instrument, InstrumentId, Lots, Order, OrderId,
    OrderRequest, OrderStatus, Price, Side,
};
use crate::ledger::{Balance, Ledger, LedgerError, Transfer};

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
    Rested {
        instrument: InstrumentId,
        order: OrderId,
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

    /// # Panics
    /// Nunca por entrada: a reserva cobre todo débito da liquidação e a oferta limitada
    /// do ledger impede estouro nos créditos. Se a liquidação falhar, o invariante
    /// quebrou e parar é o certo (fail-stop). O aceite também não falha: a ordem acabou
    /// de nascer do pedido e só pode estar em `New`.
    fn submit(&mut self, request: OrderRequest) -> Vec<Event> {
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

        if let Some(price) = order.limit_price()
            && !spec.accepts_price(price)
        {
            return reject(RejectReason::OffTick {
                price,
                tick: spec.tick(),
            });
        }
        if listing.book.contains(id) {
            return reject(RejectReason::DuplicateOrderId);
        }
        let mut order = match pre_trade_risk(order, &listing.book, listing.risk) {
            Ok(order) => order,
            Err(reason) => return reject(reason),
        };
        order
            .transition(OrderStatus::Accepted)
            .expect("ordem recém-construída está em New, e New → Accepted é legal");
        let (asset, per_lot) = match reservation_per_lot(&order, &spec, self.fees.taker_bps) {
            Ok(reservation) => reservation,
            Err(reason) => return reject(reason),
        };
        let Some(required) = per_lot.times(order.remaining()) else {
            return reject(RejectReason::ValueOverflow);
        };
        match self.ledger.reserve(account, asset, required) {
            Ok(()) => {}
            Err(LedgerError::InsufficientAvailable { available, .. }) => {
                return reject(RejectReason::InsufficientFunds {
                    asset,
                    available,
                    required,
                });
            }
            Err(other) => unreachable!("oferta limitada impede estouro do reservado: {other:?}"),
        }
        self.reservations.insert(
            (instrument, id),
            Reservation {
                account,
                asset,
                amount: required,
                per_lot,
            },
        );

        let taker = Taker {
            id,
            side: order.side(),
        };
        let outcome = listing.book.submit(order);

        let mut events = Vec::new();
        for fill in outcome.fills {
            let fee = settle_fill(
                &mut self.ledger,
                &mut self.reservations,
                &spec,
                self.fees,
                taker,
                &fill,
            );
            events.push(Event::Fill {
                instrument,
                fill,
                fee,
            });
        }

        events.extend(close_leftover(
            &mut self.ledger,
            &mut self.reservations,
            (instrument, id),
            outcome.leftover,
        ));
        events
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
        let event = match listing.book.cancel(order, account) {
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
    order: &Order,
    instrument: &Instrument,
    taker_bps: Bps,
) -> Result<(AssetId, Amount), RejectReason> {
    match (order.side(), order.price_bound()) {
        (Side::Bid, Some(bound)) => {
            let price = Amount::new(bound.get()).expect("preço é positivo");
            price
                .checked_add(taker_bps.fee_on(price))
                .map(|per_lot| (instrument.quote(), per_lot))
                .ok_or(RejectReason::ValueOverflow)
        }
        (Side::Bid, None) => unreachable!("o risco pré-trade dá proteção a toda ordem a mercado"),
        (Side::Ask, _) => Ok((instrument.base(), instrument.lot_size())),
    }
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
    leftover: Leftover,
) -> Option<Event> {
    let (instrument, order) = key;
    match leftover {
        Leftover::None => None,
        Leftover::Rested(lots) => Some(Event::Rested {
            instrument,
            order,
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
                lots: lots(5),
            }]
        );
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_bid(), Price::new(100).ok());
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
}
