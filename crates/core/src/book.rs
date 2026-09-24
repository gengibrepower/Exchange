use std::collections::{BTreeMap, VecDeque};

use crate::domain::{Fill, Lots, Order, Price, Side};

#[derive(Default)]
pub struct OrderBook {
    bids: BTreeMap<Price, VecDeque<Order>>,
    asks: BTreeMap<Price, VecDeque<Order>>,
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

    fn rest(&mut self, order: Order) {
        let side = match order.side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        side.entry(order.price).or_default().push_back(order);
    }
}

pub fn match_order(book: &mut OrderBook, mut taker: Order) -> Vec<Fill> {
    let mut fills = Vec::new();

    while taker.remaining > Lots::ZERO {
        let best = match taker.side {
            Side::Bid => book.best_ask(),
            Side::Ask => book.best_bid(),
        };
        let Some(maker_price) = best else { break };
        let crosses = match taker.side {
            Side::Bid => taker.price >= maker_price,
            Side::Ask => taker.price <= maker_price,
        };
        if !crosses {
            break;
        }
        let opposite = match taker.side {
            Side::Bid => &mut book.asks,
            Side::Ask => &mut book.bids,
        };
        let Some(level) = opposite.get_mut(&maker_price) else {
            break;
        };
        let Some(maker) = level.front_mut() else {
            break;
        };
        let traded = taker.remaining.min(maker.remaining);
        fills.push(Fill {
            taker: taker.id,
            maker: maker.id,
            price: maker_price,
            lots: traded,
        });
        taker.remaining -= traded;
        maker.remaining -= traded;
        let maker_filled = maker.remaining == Lots::ZERO;
        if maker_filled {
            level.pop_front();
        }
        if level.is_empty() {
            opposite.remove(&maker_price);
        }
    }
    if taker.remaining > Lots::ZERO {
        book.rest(taker);
    }
    fills
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;
    use crate::domain::{AccountId, Fill, InstrumentId, OrderId, OrderStatus, Seq};

    fn order(id: u64, side: Side, price: i64, lots: i64) -> Order {
        Order {
            id: OrderId::new(id),
            account: AccountId::new(1),
            instrument: InstrumentId::new(1),
            side,
            price: Price::new(price),
            total: Lots::new(lots),
            remaining: Lots::new(lots),
            seq: Seq::new(id),
            status: OrderStatus::New,
        }
    }

    fn resting_ids(level: Option<&VecDeque<Order>>) -> Vec<u64> {
        level
            .map(|orders| orders.iter().map(|order| order.id.get()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn taker_rests_when_book_is_empty() {
        let mut book = OrderBook::new();

        let fills = match_order(&mut book, order(1, Side::Bid, 100, 5));

        assert!(fills.is_empty());
        assert_eq!(book.best_bid(), Some(Price::new(100)));
        assert_eq!(resting_ids(book.bids.get(&Price::new(100))), vec![1]);
    }

    #[test]
    fn taker_rests_when_prices_do_not_cross() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 101, 5));

        let fills = match_order(&mut book, order(2, Side::Bid, 100, 5));

        assert!(fills.is_empty());
        assert_eq!(book.best_bid(), Some(Price::new(100)));
        assert_eq!(book.best_ask(), Some(Price::new(101)));
        assert_eq!(resting_ids(book.asks.get(&Price::new(101))), vec![1]);
    }

    #[test]
    fn exact_match_empties_level() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 5));

        let fills = match_order(&mut book, order(2, Side::Bid, 100, 5));

        assert_eq!(
            fills,
            vec![Fill {
                taker: OrderId::new(2),
                maker: OrderId::new(1),
                price: Price::new(100),
                lots: Lots::new(5),
            }]
        );

        assert_eq!(book.best_ask(), None);

        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn partial_fill_keeps_maker_at_front() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 10));

        let fills = match_order(&mut book, order(2, Side::Bid, 100, 5));

        assert_eq!(
            fills,
            vec![Fill {
                taker: OrderId::new(2),
                maker: OrderId::new(1),
                price: Price::new(100),
                lots: Lots::new(5),
            }]
        );

        assert_eq!(resting_ids(book.asks.get(&Price::new(100))), vec![1]);

        assert_eq!(
            book.asks[&Price::new(100)].front().unwrap().remaining,
            Lots::new(5)
        );

        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn sweeps_levels_at_maker_price() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 2));
        book.rest(order(2, Side::Ask, 101, 2));

        let fills = match_order(&mut book, order(3, Side::Bid, 105, 5));

        assert_eq!(
            fills,
            vec![
                Fill {
                    taker: OrderId::new(3),
                    maker: OrderId::new(1),
                    price: Price::new(100),
                    lots: Lots::new(2)
                },
                Fill {
                    taker: OrderId::new(3),
                    maker: OrderId::new(2),
                    price: Price::new(101),
                    lots: Lots::new(2)
                },
            ]
        );

        assert_eq!(book.best_ask(), None);

        assert_eq!(book.best_bid(), Some(Price::new(105)));
    }

    #[test]
    fn same_level_matches_fifo() {
        let mut book = OrderBook::new();
        book.rest(order(1, Side::Ask, 100, 3));
        book.rest(order(2, Side::Ask, 100, 3));

        let fills = match_order(&mut book, order(3, Side::Bid, 100, 3));

        assert_eq!(
            fills,
            vec![Fill {
                taker: OrderId::new(3),
                maker: OrderId::new(1),
                price: Price::new(100),
                lots: Lots::new(3),
            }]
        );

        assert_eq!(resting_ids(book.asks.get(&Price::new(100))), vec![2]);
    }
}
