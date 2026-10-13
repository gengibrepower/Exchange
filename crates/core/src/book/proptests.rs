use proptest::prelude::*;

use super::reference::ReferenceBook;
use super::tests::{order, protected_market};
use super::{Leftover, OrderBook};
use crate::domain::{AccountId, Lots, Order, OrderId, OrderStatus, Side, TimeInForce};

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

// GTC pesa mais para o livro encher; sem isso IOC e FOK quase nunca achariam contraparte.
fn time_in_force() -> impl Strategy<Value = TimeInForce> {
    prop_oneof![
        3 => Just(TimeInForce::Gtc),
        1 => Just(TimeInForce::Ioc),
        1 => Just(TimeInForce::Fok),
    ]
}

// `None` é ordem limite; `Some(proteção)` é ordem a mercado, protegida ou não.
fn market_kind() -> impl Strategy<Value = Option<Option<i64>>> {
    prop_oneof![
        4 => Just(None),
        1 => Just(Some(None)),
        1 => (95..=105_i64).prop_map(|bound| Some(Some(bound))),
    ]
}

// Faixa de preço estreita de propósito: com preços espalhados quase nada cruzaria
// e as propriedades passariam sem exercitar o matching.
fn order_flow() -> impl Strategy<Value = Vec<Order>> {
    let spec = (
        side(),
        95..=105_i64,
        1..=10_i64,
        time_in_force(),
        market_kind(),
    );
    prop::collection::vec(spec, 1..=50).prop_map(|specs| {
        (1..)
            .zip(specs)
            .map(|(id, (side, price, lots, tif, market))| {
                let order = match market {
                    Some(protection) => protected_market(id, side, lots, protection),
                    None => order(id, side, price, lots),
                };
                order.with_time_in_force(tif)
            })
            .collect()
    })
}

fn book_state(book: &OrderBook) -> Vec<(OrderId, Lots)> {
    book.bids
        .values()
        .chain(book.asks.values())
        .flatten()
        .map(|order| (order.id(), order.remaining()))
        .collect()
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

// Fluxo completo para o oráculo: limite e mercado, GTC/IOC/FOK, e cancelamentos que
// às vezes acertam ordem descansando, às vezes ordem já executada ou cancelada.
fn mixed_op_flow() -> impl Strategy<Value = Vec<Op>> {
    let spec = (
        prop_oneof![4 => Just(false), 1 => Just(true)],
        side(),
        95..=105_i64,
        1..=10_i64,
        time_in_force(),
        market_kind(),
        any::<prop::sample::Index>(),
    );
    prop::collection::vec(spec, 1..=80).prop_map(|specs| {
        let mut next_id = 1;
        specs
            .into_iter()
            .map(|(is_cancel, side, price, lots, tif, market, target)| {
                if is_cancel && next_id > 1 {
                    return Op::Cancel(OrderId::new(1 + target.index(next_id - 1) as u64));
                }
                let id = next_id as u64;
                next_id += 1;
                let order = match market {
                    Some(protection) => protected_market(id, side, lots, protection),
                    None => order(id, side, price, lots),
                };
                Op::Submit(order.with_time_in_force(tif))
            })
            .collect()
    })
}

fn sorted_state(orders: impl Iterator<Item = (OrderId, Lots)>) -> Vec<(u64, i64)> {
    let mut state: Vec<_> = orders.map(|(id, lots)| (id.get(), lots.get())).collect();
    state.sort_unstable();
    state
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
    #![proptest_config(ProptestConfig::with_cases(2_000))]

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
    fn only_gtc_limit_orders_rest_at_their_own_price(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            book.submit(order);
            for (price, level) in book.bids.iter().chain(book.asks.iter()) {
                for resting in level {
                    prop_assert_eq!(resting.time_in_force(), TimeInForce::Gtc);
                    prop_assert_eq!(resting.limit_price(), Some(*price));
                }
            }
        }
    }

    #[test]
    fn taker_lots_are_conserved(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            let total = order.total().get();
            let outcome = book.submit(order);
            let filled: i64 = outcome.fills.iter().map(|fill| fill.lots.get()).sum();
            let left = match outcome.leftover {
                Leftover::None => 0,
                Leftover::Rested(lots) | Leftover::Cancelled(lots) | Leftover::Expired(lots) => {
                    lots.get()
                }
            };
            prop_assert_eq!(filled + left, total);
        }
    }

    #[test]
    fn fok_is_all_or_nothing(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            let is_fok = order.time_in_force() == TimeInForce::Fok;
            let total = order.total().get();
            let before = book_state(&book);
            let outcome = book.submit(order);
            if !is_fok {
                continue;
            }
            let filled: i64 = outcome.fills.iter().map(|fill| fill.lots.get()).sum();
            if filled == total {
                prop_assert_eq!(outcome.leftover, Leftover::None);
            } else {
                prop_assert_eq!(filled, 0, "FOK executou parcialmente");
                prop_assert_eq!(outcome.leftover, Leftover::Expired(Lots::new(total).unwrap()));
                prop_assert_eq!(book_state(&book), before, "FOK expirada mexeu no livro");
            }
        }
    }

    // Oráculo: a mesma ordem como IOC num clone do livro executa tudo o que a liquidez
    // permite, sem consulta prévia. A FOK tem que executar exatamente quando a IOC
    // executaria tudo — "tudo ou nada" sozinho não pega uma FOK que expira por engano.
    #[test]
    fn fok_expires_only_without_liquidity(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            if order.time_in_force() != TimeInForce::Fok {
                book.submit(order);
                continue;
            }
            let total = order.total().get();
            let mut oracle = book.clone();
            let probe = oracle.submit(order.clone().with_time_in_force(TimeInForce::Ioc));
            let available: i64 = probe.fills.iter().map(|fill| fill.lots.get()).sum();
            let outcome = book.submit(order);
            let filled: i64 = outcome.fills.iter().map(|fill| fill.lots.get()).sum();
            prop_assert_eq!(
                filled == total,
                available == total,
                "FOK discordou do oráculo: executou {}, liquidez cobria {} de {}",
                filled,
                available,
                total
            );
        }
    }

    #[test]
    fn matches_reference_model(ops in mixed_op_flow()) {
        let mut book = OrderBook::new();
        let mut reference = ReferenceBook::default();
        for op in ops {
            match op {
                Op::Submit(order) => {
                    let expected = reference.submit(order.clone());
                    let actual = book.submit(order);
                    prop_assert_eq!(actual, expected);
                }
                Op::Cancel(id) => {
                    let expected = reference.cancel(id, AccountId::new(1)).map(|o| o.remaining());
                    let actual = book.cancel(id, AccountId::new(1)).map(|o| o.remaining());
                    prop_assert_eq!(actual, expected);
                }
            }
            prop_assert_eq!(
                sorted_state(book_state(&book).into_iter()),
                sorted_state(reference.resting().iter().map(|o| (o.id(), o.remaining())))
            );
        }
    }

    // Seção 3.5, "prioridade respeitada", dita com as palavras dela: nos fills de um
    // taker o preço nunca piora e, no mesmo preço, os makers saem por ordem de chegada.
    // Nos geradores o Seq de cada ordem é igual ao seu id, então comparar ids basta.
    #[test]
    fn fills_respect_price_time_priority(orders in order_flow()) {
        let mut book = OrderBook::new();
        for order in orders {
            let side = order.side();
            let outcome = book.submit(order);
            for pair in outcome.fills.windows(2) {
                let (earlier, later) = (&pair[0], &pair[1]);
                let price_never_worsens = match side {
                    Side::Bid => earlier.price <= later.price,
                    Side::Ask => earlier.price >= later.price,
                };
                prop_assert!(price_never_worsens, "preço piorou: {earlier:?} depois {later:?}");
                if earlier.price == later.price {
                    prop_assert!(
                        earlier.maker.get() < later.maker.get(),
                        "FIFO furado no preço {:?}: {:?} antes de {:?}",
                        earlier.price,
                        earlier.maker,
                        later.maker
                    );
                }
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
