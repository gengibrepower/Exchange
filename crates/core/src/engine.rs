use std::collections::HashMap;

use crate::book::{CancelError, Leftover, OrderBook};
use crate::domain::{
    AccountId, Amount, AssetId, Fill, Instrument, InstrumentId, Lots, Order, OrderId, OrderStatus,
    Price, Side,
};
use crate::ledger::{Balance, Ledger, LedgerError, Transfer};

#[derive(Clone, Debug)]
pub enum Command {
    Deposit {
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    },
    Submit(Order),
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
    NotNew(OrderStatus),
    OffTick {
        price: Price,
        tick: Price,
    },
    DuplicateOrderId,
    MarketBuyNeedsProtection,
    ValueOverflow,
    InsufficientFunds {
        asset: AssetId,
        available: Amount,
        required: Amount,
    },
    UnknownOrder,
    NotOwner,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    DuplicateInstrument(InstrumentId),
}

struct Listing {
    instrument: Instrument,
    book: OrderBook,
}

/// O que uma ordem aberta ainda trava no ledger. Soma, por conta e ativo, igual ao
/// `reserved` do ledger — é a invariante "reserva consistente" da seção 3.5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Reservation {
    account: AccountId,
    asset: AssetId,
    amount: Amount,
}

/// As duas pernas de um negócio: base do vendedor para o comprador, quote do comprador
/// para o vendedor. Taxa entra como terceira perna na PR seguinte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FillSettlement {
    base: Transfer,
    quote: Transfer,
}

pub struct Engine {
    listings: HashMap<InstrumentId, Listing>,
    ledger: Ledger,
    reservations: Reservations,
}

impl Engine {
    /// # Errors
    /// [`ConfigError::DuplicateInstrument`] se o mesmo id aparecer duas vezes.
    pub fn new(instruments: impl IntoIterator<Item = Instrument>) -> Result<Self, ConfigError> {
        let mut listings = HashMap::new();
        for instrument in instruments {
            let id = instrument.id();
            let listing = Listing {
                instrument,
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
    /// quebrou e parar é o certo (fail-stop).
    fn submit(&mut self, mut order: Order) -> Vec<Event> {
        let id = order.id();
        let instrument = order.instrument();
        let account = order.account();
        let reject = |reason| vec![Event::Rejected { order: id, reason }];

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
        if order.transition(OrderStatus::Accepted).is_err() {
            return reject(RejectReason::NotNew(order.status()));
        }
        let (asset, required) = match required_reservation(&order, &spec) {
            Ok(reservation) => reservation,
            Err(reason) => return reject(reason),
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
            },
        );

        let side = order.side();
        let limit = order.price_bound();
        let outcome = listing.book.submit(order);

        let taker = Taker {
            id,
            account,
            side,
            limit,
        };
        let mut events = Vec::new();
        for fill in outcome.fills {
            settle_fill(
                &mut self.ledger,
                &mut self.reservations,
                &spec,
                taker,
                &fill,
            );
            events.push(Event::Fill { instrument, fill });
        }

        match outcome.leftover {
            Leftover::None => {}
            Leftover::Rested(lots) => events.push(Event::Rested {
                instrument,
                order: id,
                lots,
            }),
            Leftover::Cancelled(lots) => {
                release_rest(&mut self.ledger, &mut self.reservations, (instrument, id));
                events.push(Event::Cancelled {
                    instrument,
                    order: id,
                    lots,
                });
            }
            Leftover::Expired(lots) => {
                release_rest(&mut self.ledger, &mut self.reservations, (instrument, id));
                events.push(Event::Expired {
                    instrument,
                    order: id,
                    lots,
                });
            }
        }
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
    account: AccountId,
    side: Side,
    limit: Option<Price>,
}

/// Liquida um fill: as duas pernas no ledger, o abatimento das reservas de comprador e
/// vendedor e, para comprador taker com limite, a devolução da melhoria de preço.
fn settle_fill(
    ledger: &mut Ledger,
    reservations: &mut Reservations,
    spec: &Instrument,
    taker: Taker,
    fill: &Fill,
) {
    let instrument = spec.id();
    let maker_account = reservations[&(instrument, fill.maker)].account;
    let (buyer, buyer_order, seller, seller_order) = match taker.side {
        Side::Bid => (taker.account, taker.id, maker_account, fill.maker),
        Side::Ask => (maker_account, fill.maker, taker.account, taker.id),
    };
    let settlement = fill_settlement(spec, fill, buyer, seller);
    ledger
        .settle(&[settlement.base, settlement.quote])
        .expect("reserva cobre o débito e oferta limitada impede estouro");
    consume(
        reservations,
        (instrument, buyer_order),
        settlement.quote.amount,
    );
    consume(
        reservations,
        (instrument, seller_order),
        settlement.base.amount,
    );

    if taker.side == Side::Bid
        && let Some(limit) = taker.limit
    {
        let reserved = limit.times(fill.lots).expect("cabia na reserva");
        let improvement = reserved
            .checked_sub(settlement.quote.amount)
            .expect("maker cruzou, então não é mais caro que o limite");
        if improvement > Amount::ZERO {
            ledger
                .release(taker.account, spec.quote(), improvement)
                .expect("melhoria de preço estava reservada");
            consume(reservations, (instrument, taker.id), improvement);
        }
    }
}

fn required_reservation(
    order: &Order,
    instrument: &Instrument,
) -> Result<(AssetId, Amount), RejectReason> {
    match (order.side(), order.price_bound()) {
        (Side::Bid, Some(bound)) => bound
            .times(order.remaining())
            .map(|amount| (instrument.quote(), amount))
            .ok_or(RejectReason::ValueOverflow),
        (Side::Bid, None) => Err(RejectReason::MarketBuyNeedsProtection),
        (Side::Ask, _) => instrument
            .lot_size()
            .times(order.remaining())
            .map(|amount| (instrument.base(), amount))
            .ok_or(RejectReason::ValueOverflow),
    }
}

/// As duas transferências que liquidam um fill (seção 4.2, sem a taxa).
fn fill_settlement(
    instrument: &Instrument,
    fill: &Fill,
    buyer: AccountId,
    seller: AccountId,
) -> FillSettlement {
    let base = instrument
        .lot_size()
        .times(fill.lots)
        .expect("já coube na reserva do vendedor, calculada sem estourar");
    let quote = fill
        .price
        .times(fill.lots)
        .expect("não passa da reserva do comprador, calculada sem estourar");
    FillSettlement {
        base: Transfer {
            asset: instrument.base(),
            from: seller,
            to: buyer,
            amount: base,
        },
        quote: Transfer {
            asset: instrument.quote(),
            from: buyer,
            to: seller,
            amount: quote,
        },
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
    use crate::domain::{Seq, TimeInForce};

    const TESTE_BRL: InstrumentId = InstrumentId::new(1);
    const OUTRO_BRL: InstrumentId = InstrumentId::new(2);
    const BRL: AssetId = AssetId::new(1);
    const TESTE: AssetId = AssetId::new(10);
    const ALICE: AccountId = AccountId::new(1);
    const BOB: AccountId = AccountId::new(2);
    const PLENTY: i64 = 1_000_000_000;

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
        let mut engine = Engine::new(specs.iter().copied()).expect("ids de teste distintos");
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

    fn order_for(account: AccountId, id: u64, side: Side, price: i64, quantity: i64) -> Order {
        Order::new(
            OrderId::new(id),
            account,
            TESTE_BRL,
            side,
            Price::new(price).expect("preço de teste válido"),
            lots(quantity),
            Seq::new(id),
        )
        .expect("ordem de teste válida")
    }

    fn market_for(account: AccountId, id: u64, side: Side, quantity: i64) -> Order {
        protected_for(account, id, side, quantity, None)
    }

    fn protected_for(
        account: AccountId,
        id: u64,
        side: Side,
        quantity: i64,
        protection: Option<i64>,
    ) -> Order {
        Order::market(
            OrderId::new(id),
            account,
            TESTE_BRL,
            side,
            lots(quantity),
            Seq::new(id),
            protection.map(|bound| Price::new(bound).expect("proteção de teste válida")),
        )
        .expect("ordem de teste válida")
    }

    fn submit(engine: &mut Engine, order: Order) -> Vec<Event> {
        engine.apply(Command::Submit(order))
    }

    // Engine "pobre": Alice tem 1000 BRL, Bob tem 10 lots de TESTE (lot de 100 unidades).
    fn lean_engine() -> Engine {
        let mut engine = Engine::new([spec_with_lot(TESTE_BRL, 1, 100)]).expect("config válida");
        deposit(&mut engine, ALICE, BRL, 1_000);
        deposit(&mut engine, BOB, TESTE, 1_000);
        engine
    }

    #[test]
    fn deposit_credits_available() {
        let mut engine = Engine::new([spec(TESTE_BRL, 1)]).expect("config válida");

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
        let mut engine = Engine::new([spec(TESTE_BRL, 1)]).expect("config válida");
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
            order_for(ALICE, 2, Side::Bid, 100, 3).with_time_in_force(TimeInForce::Ioc),
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(900, 0));

        submit(
            &mut engine,
            order_for(ALICE, 3, Side::Bid, 100, 5).with_time_in_force(TimeInForce::Fok),
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(900, 0));
    }

    #[test]
    fn market_buy_needs_protection() {
        let mut engine = lean_engine();

        let events = submit(&mut engine, market_for(ALICE, 1, Side::Bid, 1));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::MarketBuyNeedsProtection,
            }]
        );
        assert_eq!(engine.balance(ALICE, BRL), balance(1_000, 0));
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
        let result = Engine::new([spec(TESTE_BRL, 1), spec(TESTE_BRL, 5)]);

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

    fn order(id: u64, instrument: InstrumentId, side: Side, price: i64, lots: i64) -> Order {
        Order::new(
            OrderId::new(id),
            AccountId::new(1),
            instrument,
            side,
            Price::new(price).expect("preço de teste válido"),
            Lots::new(lots).expect("lots de teste válidos"),
            Seq::new(id),
        )
        .expect("ordem de teste válida")
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
    fn order_not_new_is_rejected() {
        let mut engine = engine(&[TESTE_BRL]);
        let mut accepted = order(1, TESTE_BRL, Side::Bid, 100, 5);
        accepted
            .transition(OrderStatus::Accepted)
            .expect("ordem nova pode ser aceita");

        let events = engine.apply(Command::Submit(accepted));

        assert_eq!(
            events,
            vec![Event::Rejected {
                order: OrderId::new(1),
                reason: RejectReason::NotNew(OrderStatus::Accepted),
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
        let ioc = order(2, TESTE_BRL, Side::Bid, 100, 5).with_time_in_force(TimeInForce::Ioc);

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
        let fok = order(2, TESTE_BRL, Side::Bid, 100, 5).with_time_in_force(TimeInForce::Fok);

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
