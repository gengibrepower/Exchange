use std::collections::HashMap;

use crate::book::{CancelError, Leftover, OrderBook};
use crate::domain::{
    AccountId, Fill, Instrument, InstrumentId, Lots, Order, OrderId, OrderStatus, Price,
};

#[derive(Clone, Debug)]
pub enum Command {
    Submit(Order),
    Cancel {
        instrument: InstrumentId,
        account: AccountId,
        order: OrderId,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
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
    OffTick { price: Price, tick: Price },
    DuplicateOrderId,
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

pub struct Engine {
    listings: HashMap<InstrumentId, Listing>,
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
        Ok(Self { listings })
    }

    pub fn instrument(&self, id: InstrumentId) -> Option<&Instrument> {
        self.listings.get(&id).map(|listing| &listing.instrument)
    }

    pub fn book(&self, instrument: InstrumentId) -> Option<&OrderBook> {
        self.listings.get(&instrument).map(|listing| &listing.book)
    }

    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        match command {
            Command::Submit(mut order) => {
                let id = order.id();
                let instrument = order.instrument();

                let Some(listing) = self.listings.get_mut(&instrument) else {
                    return vec![Event::Rejected {
                        order: id,
                        reason: RejectReason::UnknownInstrument(instrument),
                    }];
                };
                let tick = listing.instrument.tick();
                let book = &mut listing.book;

                if let Some(price) = order.limit_price()
                    && !listing.instrument.accepts_price(price)
                {
                    return vec![Event::Rejected {
                        order: id,
                        reason: RejectReason::OffTick { price, tick },
                    }];
                }

                if book.contains(id) {
                    return vec![Event::Rejected {
                        order: id,
                        reason: RejectReason::DuplicateOrderId,
                    }];
                }

                if order.transition(OrderStatus::Accepted).is_err() {
                    return vec![Event::Rejected {
                        order: id,
                        reason: RejectReason::NotNew(order.status()),
                    }];
                }

                let outcome = book.submit(order);

                let mut events = Vec::new();
                for fill in outcome.fills {
                    events.push(Event::Fill { instrument, fill });
                }
                match outcome.leftover {
                    Leftover::None => {}
                    Leftover::Rested(lots) => events.push(Event::Rested {
                        instrument,
                        order: id,
                        lots,
                    }),
                    Leftover::Cancelled(lots) => events.push(Event::Cancelled {
                        instrument,
                        order: id,
                        lots,
                    }),
                    Leftover::Expired(lots) => events.push(Event::Expired {
                        instrument,
                        order: id,
                        lots,
                    }),
                }
                events
            }
            Command::Cancel {
                instrument,
                account,
                order,
            } => {
                let Some(listing) = self.listings.get_mut(&instrument) else {
                    return vec![Event::Rejected {
                        order,
                        reason: RejectReason::UnknownInstrument(instrument),
                    }];
                };
                let event = match listing.book.cancel(order, account) {
                    Ok(cancelled) => Event::Cancelled {
                        instrument,
                        order,
                        lots: cancelled.remaining(),
                    },
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountId, Amount, AssetId, Seq, Side, TimeInForce};

    const TESTE_BRL: InstrumentId = InstrumentId::new(1);
    const OUTRO_BRL: InstrumentId = InstrumentId::new(2);

    fn spec(id: InstrumentId, tick: i64) -> Instrument {
        Instrument::new(
            id,
            AssetId::new(id.get() * 10),
            AssetId::new(1),
            Price::new(tick).expect("tick de teste válido"),
            Amount::new(1).expect("lot de teste válido"),
        )
        .expect("instrumento de teste válido")
    }

    fn engine(ids: &[InstrumentId]) -> Engine {
        Engine::new(ids.iter().map(|id| spec(*id, 1))).expect("ids de teste distintos")
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
        let engine = Engine::new([spec(TESTE_BRL, 10)]).expect("config válida");

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
        let mut engine = Engine::new([spec(TESTE_BRL, 10)]).expect("config válida");

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
        let mut engine = Engine::new([spec(TESTE_BRL, 10)]).expect("config válida");

        let events = engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Bid, 110, 1)));

        assert!(matches!(events.as_slice(), [Event::Rested { .. }]));
    }

    #[test]
    fn market_order_has_no_price_to_check() {
        let mut engine = Engine::new([spec(TESTE_BRL, 10)]).expect("config válida");
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 110, 1)));
        let market = Order::market(
            OrderId::new(2),
            AccountId::new(1),
            TESTE_BRL,
            Side::Bid,
            Lots::new(1).expect("lots de teste válidos"),
            Seq::new(2),
        )
        .expect("ordem de teste válida");

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
