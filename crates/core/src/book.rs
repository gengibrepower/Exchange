use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::domain::{AccountId, Fill, Lots, Order, OrderId, OrderStatus, Price, Side, TimeInForce};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Leftover {
    None,
    Rested(Lots),
    Cancelled(Lots),
    Expired(Lots),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MatchOutcome {
    pub fills: Vec<Fill>,
    pub leftover: Leftover,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelError {
    UnknownOrder,
    NotOwner,
}

#[derive(Default)]
#[cfg_attr(test, derive(Clone))]
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
    /// A sobra segue o time-in-force: GTC descansa, IOC cancela; FOK sem liquidez para
    /// tudo expira sem tocar no livro.
    ///
    /// # Panics
    /// Se o taker não estiver aceito. Fora isso, nunca: makers no livro estão sempre
    /// aceitos, `traded` é positivo e não excede nenhum dos dois restantes, e uma FOK só
    /// executa quando a liquidez cobre tudo. Se algo disso falhar, o invariante quebrou e
    /// parar é o certo (fail-stop).
    pub(crate) fn submit(&mut self, mut taker: Order) -> MatchOutcome {
        if taker.time_in_force() == TimeInForce::Fok && !self.can_fill_completely(&taker) {
            let lots = taker.remaining();
            taker
                .transition(OrderStatus::Expired)
                .expect("FOK aceita e sem fill pode expirar");
            return MatchOutcome {
                fills: Vec::new(),
                leftover: Leftover::Expired(lots),
            };
        }

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
        let leftover = if remaining == Lots::ZERO {
            Leftover::None
        } else {
            match taker.time_in_force() {
                TimeInForce::Gtc => {
                    self.rest(taker);
                    Leftover::Rested(remaining)
                }
                TimeInForce::Ioc => {
                    taker
                        .transition(OrderStatus::Cancelled)
                        .expect("sobra de IOC aceita ou parcial pode ser cancelada");
                    Leftover::Cancelled(remaining)
                }
                TimeInForce::Fok => unreachable!("FOK só executa quando a liquidez cobre tudo"),
            }
        };
        MatchOutcome { fills, leftover }
    }

    /// Consulta só de leitura: o lado oposto, nos níveis que cruzam o preço do taker,
    /// tem quantidade suficiente para executar o restante inteiro?
    fn can_fill_completely(&self, taker: &Order) -> bool {
        let mut needed = taker.remaining();
        match taker.side() {
            Side::Bid => {
                for (price, level) in &self.asks {
                    if *price > taker.price() {
                        break;
                    }
                    for maker in level {
                        match needed.checked_sub(maker.remaining()) {
                            Some(rest) if rest > Lots::ZERO => needed = rest,
                            _ => return true,
                        }
                    }
                }
            }
            Side::Ask => {
                for (price, level) in self.bids.iter().rev() {
                    if *price < taker.price() {
                        break;
                    }
                    for maker in level {
                        match needed.checked_sub(maker.remaining()) {
                            Some(rest) if rest > Lots::ZERO => needed = rest,
                            _ => return true,
                        }
                    }
                }
            }
        }
        false
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

    fn with_tif(order: Order, time_in_force: TimeInForce) -> Order {
        order.with_time_in_force(time_in_force)
    }

    fn total_filled(fills: &[Fill]) -> i64 {
        fills.iter().map(|fill| fill.lots.get()).sum()
    }

    fn book_state(book: &OrderBook) -> Vec<(u64, i64)> {
        book.bids
            .values()
            .chain(book.asks.values())
            .flatten()
            .map(|order| (order.id().get(), order.remaining().get()))
            .collect()
    }

    #[test]
    fn can_fill_completely_sums_crossing_levels() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));
        book.rest(order(2, Side::Ask, 101, 3));
        book.rest(order(3, Side::Ask, 102, 10));

        assert!(book.can_fill_completely(&order(4, Side::Bid, 101, 5)));
        assert!(!book.can_fill_completely(&order(4, Side::Bid, 101, 6)));
        assert!(book.can_fill_completely(&order(4, Side::Bid, 102, 15)));
        assert!(!book.can_fill_completely(&order(4, Side::Bid, 99, 1)));
    }

    #[test]
    fn can_fill_completely_reads_bids_from_highest() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 2));
        book.rest(order(2, Side::Bid, 98, 5));

        assert!(book.can_fill_completely(&order(3, Side::Ask, 99, 2)));
        assert!(!book.can_fill_completely(&order(3, Side::Ask, 99, 3)));
        assert!(book.can_fill_completely(&order(3, Side::Ask, 98, 7)));
    }

    #[test]
    fn can_fill_completely_is_false_on_empty_side() {
        let book = OrderBook::new();

        assert!(!book.can_fill_completely(&order(1, Side::Bid, 100, 1)));
    }

    #[test]
    fn fok_with_enough_liquidity_fills_across_levels() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));
        book.rest(order(2, Side::Ask, 101, 3));

        let outcome = book.submit(with_tif(order(3, Side::Bid, 101, 5), TimeInForce::Fok));

        assert_eq!(total_filled(&outcome.fills), 5);
        assert_eq!(outcome.leftover, Leftover::None);
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn fok_without_enough_liquidity_expires_and_leaves_book_untouched() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));
        book.rest(order(2, Side::Ask, 105, 10));
        let before = book_state(&book);

        let outcome = book.submit(with_tif(order(3, Side::Bid, 101, 5), TimeInForce::Fok));

        assert_eq!(outcome.fills, []);
        assert_eq!(outcome.leftover, Leftover::Expired(lots(5)));
        assert_eq!(book_state(&book), before);
        assert!(!book.contains(OrderId::new(3)));
    }

    #[test]
    fn ioc_cancels_remainder_after_partial_fill() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));

        let outcome = book.submit(with_tif(order(2, Side::Bid, 100, 5), TimeInForce::Ioc));

        assert_eq!(total_filled(&outcome.fills), 2);
        assert_eq!(outcome.leftover, Leftover::Cancelled(lots(3)));
        assert_eq!(book.best_bid(), None);
        assert!(!book.contains(OrderId::new(2)));
    }

    #[test]
    fn ioc_without_liquidity_is_cancelled_entirely() {
        let mut book = OrderBook::new();

        let outcome = book.submit(with_tif(order(1, Side::Ask, 100, 4), TimeInForce::Ioc));

        assert_eq!(outcome.fills, []);
        assert_eq!(outcome.leftover, Leftover::Cancelled(lots(4)));
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn ioc_fully_filled_leaves_nothing() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Bid, 100, 5));

        let outcome = book.submit(with_tif(order(2, Side::Ask, 100, 5), TimeInForce::Ioc));

        assert_eq!(outcome.leftover, Leftover::None);
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
        assert_eq!(outcome.leftover, Leftover::Rested(lots(3)));
    }

    #[test]
    fn outcome_reports_nothing_rested_when_fully_filled() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 5));

        let outcome = book.submit(order(2, Side::Bid, 100, 5));

        assert_eq!(outcome.leftover, Leftover::None);
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
