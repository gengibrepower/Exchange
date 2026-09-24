use proptest::prelude::*;

use super::tests::order;
use super::{OrderBook, match_order};
use crate::domain::{Order, Side};

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

// Faixa de preço estreita de propósito: com preços espalhados quase nada cruzaria
// e as propriedades passariam sem exercitar o matching.
fn order_flow() -> impl Strategy<Value = Vec<Order>> {
    prop::collection::vec((side(), 95..=105_i64, 1..=10_i64), 1..=50).prop_map(|specs| {
        (1..)
            .zip(specs)
            .map(|(id, (side, price, lots))| order(id, side, price, lots))
            .collect()
    })
}

proptest! {
    #[test]
    fn book_is_never_crossed(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            match_order(&mut book, order);
            if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                prop_assert!(bid < ask, "livro cruzado: bid {bid:?} >= ask {ask:?}");
            }
        }
    }

    #[test]
    fn book_has_no_empty_levels(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            match_order(&mut book, order);
            prop_assert!(book.bids.values().all(|level| !level.is_empty()));
            prop_assert!(book.asks.values().all(|level| !level.is_empty()));
        }
    }
}
