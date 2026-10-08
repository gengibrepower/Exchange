use std::collections::HashMap;

use crate::book::OrderBook;
use crate::domain::{Fill, InstrumentId, Lots, Order, OrderId, OrderStatus};

#[derive(Clone, Debug)]
pub enum Command {
    Submit(Order),
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
    Rejected {
        order: OrderId,
        reason: RejectReason,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    UnknownInstrument(InstrumentId),
    NotNew(OrderStatus),
}

pub struct Engine {
    books: HashMap<InstrumentId, OrderBook>,
}

impl Engine {
    pub fn new(instruments: impl IntoIterator<Item = InstrumentId>) -> Self {
        let books = instruments
            .into_iter()
            .map(|instrument| (instrument, OrderBook::new()))
            .collect();
        Self { books }
    }

    pub fn book(&self, instrument: InstrumentId) -> Option<&OrderBook> {
        self.books.get(&instrument)
    }

    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        match command {
            Command::Submit(mut order) => {
                let id = order.id();
                let instrument = order.instrument();

                let Some(book) = self.books.get_mut(&instrument) else {
                    return vec![Event::Rejected {
                        order: id,
                        reason: RejectReason::UnknownInstrument(instrument),
                    }];
                };

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
                if let Some(lots) = outcome.rested {
                    events.push(Event::Rested {
                        instrument,
                        order: id,
                        lots,
                    });
                }
                events
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountId, Price, Seq, Side};

    const TESTE_BRL: InstrumentId = InstrumentId::new(1);
    const OUTRO_BRL: InstrumentId = InstrumentId::new(2);

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
        let mut engine = Engine::new([TESTE_BRL]);
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
        let mut engine = Engine::new([TESTE_BRL]);
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

    #[test]
    fn order_without_counterparty_rests() {
        let mut engine = Engine::new([TESTE_BRL]);

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
        let mut engine = Engine::new([TESTE_BRL]);
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
        let mut engine = Engine::new([TESTE_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));

        let events = engine.apply(Command::Submit(order(2, TESTE_BRL, Side::Bid, 100, 5)));

        assert!(matches!(events.as_slice(), [Event::Fill { .. }]));
        let book = engine.book(TESTE_BRL).expect("instrumento registrado");
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn instruments_do_not_match_each_other() {
        let mut engine = Engine::new([TESTE_BRL, OUTRO_BRL]);
        engine.apply(Command::Submit(order(1, TESTE_BRL, Side::Ask, 100, 5)));

        let events = engine.apply(Command::Submit(order(2, OUTRO_BRL, Side::Bid, 100, 5)));

        assert!(matches!(events.as_slice(), [Event::Rested { .. }]));
        let teste = engine.book(TESTE_BRL).expect("instrumento registrado");
        let outro = engine.book(OUTRO_BRL).expect("instrumento registrado");
        assert_eq!(teste.best_ask(), Price::new(100).ok());
        assert_eq!(outro.best_bid(), Price::new(100).ok());
    }
}
