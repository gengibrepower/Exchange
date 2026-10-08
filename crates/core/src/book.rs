use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::domain::{AccountId, Fill, Lots, Order, OrderId, OrderStatus, Price, Side};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MatchOutcome {
    pub fills: Vec<Fill>,
    pub rested: Option<Lots>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelError {
    UnknownOrder,
    NotOwner,
}

#[derive(Default)]
pub struct OrderBook {
    bids: BTreeMap<Price, VecDeque<Order>>,
    asks: BTreeMap<Price, VecDeque<Order>>,
    index: HashMap<OrderId, (Side, Price)>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.last_key_value().map(|(price, _)| *price)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first_key_value().map(|(price, _)| *price)
    }

    pub fn contains(&self, id: OrderId) -> bool {
        self.index.contains_key(&id)
    }

    /// Espera um taker já aceito; o [`crate::engine::Engine`] aceita antes de chamar.
    ///
    /// # Panics
    /// Se o taker não estiver aceito. Fora isso, nunca: makers no livro estão sempre
    /// aceitos e `traded` é positivo e não excede nenhum dos dois restantes. Se um `fill`
    /// falhar, o invariante quebrou e parar é o certo (fail-stop).
    pub(crate) fn submit(&mut self, mut taker: Order) -> MatchOutcome {
        let mut fills = Vec::new();

        while taker.remaining() > Lots::ZERO {
            let best = match taker.side() {
                Side::Bid => self.best_ask(),
                Side::Ask => self.best_bid(),
            };
            let Some(maker_price) = best else { break };
            let crosses = match taker.side() {
                Side::Bid => taker.price() >= maker_price,
                Side::Ask => taker.price() <= maker_price,
            };
            if !crosses {
                break;
            }
            let opposite = match taker.side() {
                Side::Bid => &mut self.asks,
                Side::Ask => &mut self.bids,
            };
            let Some(level) = opposite.get_mut(&maker_price) else {
                break;
            };
            let Some(maker) = level.front_mut() else {
                break;
            };
            let traded = taker.remaining().min(maker.remaining());
            fills.push(Fill {
                taker: taker.id(),
                maker: maker.id(),
                price: maker_price,
                lots: traded,
            });
            taker
                .fill(traded)
                .expect("taker aceito e traded dentro do restante");
            maker
                .fill(traded)
                .expect("maker no livro está aceito e traded dentro do restante");
            let maker_filled = maker.remaining() == Lots::ZERO;
            if maker_filled && let Some(filled) = level.pop_front() {
                self.index.remove(&filled.id());
            }
            if level.is_empty() {
                opposite.remove(&maker_price);
            }
        }
        let remaining = taker.remaining();
        let rested = if remaining > Lots::ZERO {
            self.rest(taker);
            Some(remaining)
        } else {
            None
        };
        MatchOutcome { fills, rested }
    }

    /// Tira do livro uma ordem descansando e a leva a `Cancelled`.
    ///
    /// # Errors
    /// Em qualquer erro, nada muda:
    /// [`CancelError::UnknownOrder`] se a ordem não estiver descansando neste livro,
    /// [`CancelError::NotOwner`] se `account` não for a dona da ordem.
    pub(crate) fn cancel(&mut self, id: OrderId, account: AccountId) -> Result<Order, CancelError> {
        let Some(&(side, price)) = self.index.get(&id) else {
            return Err(CancelError::UnknownOrder);
        };

        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = levels
            .get_mut(&price)
            .expect("índice só aponta para nível que existe");
        let position = level
            .iter()
            .position(|order| order.id() == id)
            .expect("índice só aponta para ordem que existe");

        if level[position].account() != account {
            return Err(CancelError::NotOwner);
        }

        let mut order = level
            .remove(position)
            .expect("posição veio do position() deste nível");
        if level.is_empty() {
            levels.remove(&price);
        }
        self.index.remove(&id);
        order
            .transition(OrderStatus::Cancelled)
            .expect("ordem no livro está Accepted ou PartiallyFilled");
        Ok(order)
    }

    fn rest(&mut self, order: Order) {
        self.index.insert(order.id(), (order.side(), order.price()));
        let side = match order.side() {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        side.entry(order.price()).or_default().push_back(order);
    }
}

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;
    use crate::domain::{Fill, InstrumentId, OrderStatus, Seq};

    pub(super) fn price(units_per_lot: i64) -> Price {
        Price::new(units_per_lot).expect("preço de teste válido")
    }

    pub(super) fn lots(value: i64) -> Lots {
        Lots::new(value).expect("lots de teste válidos")
    }

    pub(super) fn order(id: u64, side: Side, price_units: i64, quantity: i64) -> Order {
        let mut order = Order::new(
            OrderId::new(id),
            AccountId::new(1),
            InstrumentId::new(1),
            side,
            price(price_units),
            lots(quantity),
            Seq::new(id),
        )
        .expect("ordem de teste válida");
        order
            .transition(OrderStatus::Accepted)
            .expect("ordem nova pode ser aceita");
        order
    }

    #[test]
    fn outcome_reports_rested_remainder() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));

        let outcome = book.submit(order(2, Side::Bid, 100, 5));

        assert_eq!(outcome.fills.len(), 1);
        assert_eq!(outcome.rested, Some(lots(3)));
    }

    #[test]
    fn outcome_reports_nothing_rested_when_fully_filled() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 5));

        let outcome = book.submit(order(2, Side::Bid, 100, 5));

        assert_eq!(outcome.rested, None);
    }

    const OWNER: AccountId = AccountId::new(1);

    #[test]
    fn cancel_removes_resting_order_and_returns_it_cancelled() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 5));

        let cancelled = book.cancel(OrderId::new(1), OWNER).unwrap();

        assert_eq!(cancelled.id(), OrderId::new(1));
        assert_eq!(cancelled.status(), OrderStatus::Cancelled);
        assert_eq!(cancelled.remaining(), lots(5));
        assert_eq!(book.best_bid(), None);
        assert!(!book.contains(OrderId::new(1)));
    }

    #[test]
    fn cancel_keeps_time_priority_of_the_others() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 1));
        book.rest(order(2, Side::Ask, 100, 1));
        book.rest(order(3, Side::Ask, 100, 1));

        book.cancel(OrderId::new(2), OWNER).unwrap();

        assert_eq!(resting_ids(book.asks.get(&price(100))), vec![1, 3]);
    }

    #[test]
    fn cancel_partially_filled_order_returns_what_was_left() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 10));
        book.submit(order(2, Side::Bid, 100, 4));

        let cancelled = book.cancel(OrderId::new(1), OWNER).unwrap();

        assert_eq!(cancelled.remaining(), lots(6));
        assert_eq!(cancelled.total(), lots(10));
        assert_eq!(cancelled.status(), OrderStatus::Cancelled);
    }

    #[test]
    fn cancel_unknown_order_changes_nothing() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 5));

        let result = book.cancel(OrderId::new(99), OWNER);

        assert_eq!(result.unwrap_err(), CancelError::UnknownOrder);
        assert_eq!(resting_ids(book.bids.get(&price(100))), vec![1]);
    }

    #[test]
    fn cancel_by_another_account_changes_nothing() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 5));

        let result = book.cancel(OrderId::new(1), AccountId::new(2));

        assert_eq!(result.unwrap_err(), CancelError::NotOwner);
        assert_eq!(resting_ids(book.bids.get(&price(100))), vec![1]);
        assert!(book.contains(OrderId::new(1)));
    }

    #[test]
    fn cancel_twice_is_unknown_the_second_time() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 5));
        book.cancel(OrderId::new(1), OWNER).unwrap();

        let result = book.cancel(OrderId::new(1), OWNER);

        assert_eq!(result.unwrap_err(), CancelError::UnknownOrder);
    }

    #[test]
    fn fully_filled_maker_leaves_the_index() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 5));
        book.submit(order(2, Side::Bid, 100, 5));

        assert!(!book.contains(OrderId::new(1)));
        assert_eq!(
            book.cancel(OrderId::new(1), OWNER).unwrap_err(),
            CancelError::UnknownOrder
        );
    }

    fn resting_ids(level: Option<&VecDeque<Order>>) -> Vec<u64> {
        level
            .map(|orders| orders.iter().map(|order| order.id().get()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn taker_rests_when_book_is_empty() {
        let mut book = OrderBook::new();

        let fills = book.submit(order(1, Side::Bid, 100, 5)).fills;

        assert_eq!(fills, []);
        assert_eq!(book.best_bid(), Some(price(100)));
        assert_eq!(resting_ids(book.bids.get(&price(100))), vec![1]);
    }

    #[test]
    fn taker_rests_when_prices_do_not_cross() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 101, 5));

        let fills = book.submit(order(2, Side::Bid, 100, 5)).fills;

        assert_eq!(fills, []);
        assert_eq!(book.best_bid(), Some(price(100)));
        assert_eq!(book.best_ask(), Some(price(101)));
        assert_eq!(resting_ids(book.asks.get(&price(101))), vec![1]);
    }

    #[test]
    fn exact_match_empties_level() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 5));

        let fills = book.submit(order(2, Side::Bid, 100, 5)).fills;

        assert_eq!(
            fills,
            vec![Fill {
                taker: OrderId::new(2),
                maker: OrderId::new(1),
                price: price(100),
                lots: lots(5),
            }]
        );

        assert_eq!(book.best_ask(), None);

        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn partial_fill_keeps_maker_at_front() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 10));

        let fills = book.submit(order(2, Side::Bid, 100, 5)).fills;

        assert_eq!(
            fills,
            vec![Fill {
                taker: OrderId::new(2),
                maker: OrderId::new(1),
                price: price(100),
                lots: lots(5),
            }]
        );

        assert_eq!(resting_ids(book.asks.get(&price(100))), vec![1]);

        let maker = book.asks[&price(100)].front().unwrap();
        assert_eq!(maker.remaining(), lots(5));
        assert_eq!(maker.status(), OrderStatus::PartiallyFilled);

        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn sweeps_levels_at_maker_price() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));
        book.rest(order(2, Side::Ask, 101, 2));

        let fills = book.submit(order(3, Side::Bid, 105, 5)).fills;

        assert_eq!(
            fills,
            vec![
                Fill {
                    taker: OrderId::new(3),
                    maker: OrderId::new(1),
                    price: price(100),
                    lots: lots(2)
                },
                Fill {
                    taker: OrderId::new(3),
                    maker: OrderId::new(2),
                    price: price(101),
                    lots: lots(2)
                },
            ]
        );

        assert_eq!(book.best_ask(), None);

        assert_eq!(book.best_bid(), Some(price(105)));
    }

    #[test]
    fn same_level_matches_fifo() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 3));
        book.rest(order(2, Side::Ask, 100, 3));

        let fills = book.submit(order(3, Side::Bid, 100, 3)).fills;

        assert_eq!(
            fills,
            vec![Fill {
                taker: OrderId::new(3),
                maker: OrderId::new(1),
                price: price(100),
                lots: lots(3),
            }]
        );

        assert_eq!(resting_ids(book.asks.get(&price(100))), vec![2]);
    }

    #[test]
    fn ask_taker_sweeps_bids_from_highest() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 2));
        book.rest(order(2, Side::Bid, 101, 2));

        let fills = book.submit(order(3, Side::Ask, 95, 5)).fills;

        assert_eq!(
            fills,
            vec![
                Fill {
                    taker: OrderId::new(3),
                    maker: OrderId::new(2),
                    price: price(101),
                    lots: lots(2)
                },
                Fill {
                    taker: OrderId::new(3),
                    maker: OrderId::new(1),
                    price: price(100),
                    lots: lots(2)
                },
            ]
        );

        assert_eq!(book.best_ask(), Some(price(95)));

        assert_eq!(book.best_bid(), None);
    }
}
