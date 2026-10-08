use proptest::prelude::*;

use super::OrderBook;
use super::tests::order;
use crate::domain::{AccountId, Lots, Order, OrderId, OrderStatus, Side};

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

#[derive(Debug)]
enum Op {
    Submit(Order),
    Cancel(OrderId),
}

// O alvo do cancel é sorteado entre todos os ids já emitidos, então às vezes cai em
// ordem descansando, às vezes em ordem já executada ou cancelada — os três caminhos.
fn op_flow() -> impl Strategy<Value = Vec<Op>> {
    let spec = (
        any::<bool>(),
        side(),
        95..=105_i64,
        1..=10_i64,
        any::<prop::sample::Index>(),
    );
    prop::collection::vec(spec, 1..=80).prop_map(|specs| {
        let mut next_id = 1;
        specs
            .into_iter()
            .map(|(is_cancel, side, price, lots, target)| {
                if is_cancel && next_id > 1 {
                    Op::Cancel(OrderId::new(1 + target.index(next_id - 1) as u64))
                } else {
                    let op = Op::Submit(order(next_id as u64, side, price, lots));
                    next_id += 1;
                    op
                }
            })
            .collect()
    })
}

fn index_mirrors_book(book: &OrderBook) -> Result<(), TestCaseError> {
    let mut resting = 0;
    for (side, levels) in [(Side::Bid, &book.bids), (Side::Ask, &book.asks)] {
        for (price, level) in levels {
            for order in level {
                prop_assert_eq!(book.index.get(&order.id()), Some(&(side, *price)));
                resting += 1;
            }
        }
    }
    prop_assert_eq!(book.index.len(), resting, "índice com entradas sobrando");
    Ok(())
}

proptest! {
    #[test]
    fn book_is_never_crossed(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            book.submit(order);
            if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                prop_assert!(bid < ask, "livro cruzado: bid {bid:?} >= ask {ask:?}");
            }
        }
    }

    #[test]
    fn book_has_no_empty_levels(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            book.submit(order);
            prop_assert!(book.bids.values().all(|level| !level.is_empty()));
            prop_assert!(book.asks.values().all(|level| !level.is_empty()));
        }
    }

    #[test]
    fn resting_orders_have_status_coherent_with_remaining(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            book.submit(order);
            for resting in book.bids.values().chain(book.asks.values()).flatten() {
                let expected = if resting.remaining() == resting.total() {
                    OrderStatus::Accepted
                } else {
                    OrderStatus::PartiallyFilled
                };
                prop_assert!(resting.remaining() > Lots::ZERO);
                prop_assert_eq!(resting.status(), expected, "ordem {:?}", resting.id());
            }
        }
    }

    #[test]
    fn index_stays_in_sync_under_submits_and_cancels(ops in op_flow()) {
        let mut book = OrderBook::new();
        for op in ops {
            match op {
                Op::Submit(order) => {
                    book.submit(order);
                }
                Op::Cancel(id) => {
                    let was_resting = book.contains(id);
                    let result = book.cancel(id, AccountId::new(1));
                    prop_assert_eq!(result.is_ok(), was_resting);
                    if let Ok(cancelled) = result {
                        prop_assert_eq!(cancelled.status(), OrderStatus::Cancelled);
                        prop_assert!(!book.contains(id));
                    }
                }
            }
            index_mirrors_book(&book)?;
            prop_assert!(book.bids.values().all(|level| !level.is_empty()));
            prop_assert!(book.asks.values().all(|level| !level.is_empty()));
            if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                prop_assert!(bid < ask, "livro cruzado: bid {bid:?} >= ask {ask:?}");
            }
        }
    }
}
