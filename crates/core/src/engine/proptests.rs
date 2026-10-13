use std::collections::HashMap;

use proptest::prelude::*;

use super::{Command, Engine, Event, FeeSchedule, ListingConfig, RiskLimits};
use crate::domain::{
    AccountId, Amount, AssetId, Bps, Instrument, InstrumentId, Lots, Order, OrderId, Price, Seq,
    Side, TimeInForce,
};

const TESTE_BRL: InstrumentId = InstrumentId::new(1);
const TESTE: AssetId = AssetId::new(1);
const BRL: AssetId = AssetId::new(2);
const ACCOUNTS: [AccountId; 2] = [AccountId::new(1), AccountId::new(2)];
const HOUSE: AccountId = AccountId::new(9);
const ASSETS: [AssetId; 2] = [TESTE, BRL];

fn spec() -> Instrument {
    Instrument::new(
        TESTE_BRL,
        TESTE,
        BRL,
        Price::new(1).expect("tick positivo"),
        Amount::new(3).expect("lot positivo"),
    )
    .expect("especificação válida")
}

#[derive(Clone, Debug)]
enum Op {
    Deposit(usize, AssetId, i64),
    Submit {
        account: usize,
        side: Side,
        price: Option<i64>,
        protection: Option<i64>,
        lots: i64,
        tif: TimeInForce,
    },
    Cancel {
        account: usize,
        target: prop::sample::Index,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let side = prop_oneof![Just(Side::Bid), Just(Side::Ask)];
    let tif = prop_oneof![
        3 => Just(TimeInForce::Gtc),
        1 => Just(TimeInForce::Ioc),
        1 => Just(TimeInForce::Fok),
    ];
    let price = prop_oneof![4 => (95..=105_i64).prop_map(Some), 1 => Just(None)];
    let protection = prop_oneof![Just(None), (95..=105_i64).prop_map(Some)];
    prop_oneof![
        2 => (0..2_usize, prop_oneof![Just(TESTE), Just(BRL)], 0..=600_i64)
            .prop_map(|(account, asset, value)| Op::Deposit(account, asset, value)),
        6 => (0..2_usize, side, price, protection, 1..=10_i64, tif).prop_map(
            |(account, side, price, protection, lots, tif)| Op::Submit {
                account,
                side,
                price,
                protection,
                lots,
                tif,
            }
        ),
        2 => (0..2_usize, any::<prop::sample::Index>())
            .prop_map(|(account, target)| Op::Cancel { account, target }),
    ]
}

fn command(op: &Op, next_id: &mut u64) -> Command {
    match op {
        Op::Deposit(account, asset, value) => Command::Deposit {
            account: ACCOUNTS[*account],
            asset: *asset,
            amount: Amount::new(*value).expect("faixa não negativa"),
        },
        Op::Submit {
            account,
            side,
            price,
            protection,
            lots,
            tif,
        } => {
            let id = *next_id;
            *next_id += 1;
            let lots = Lots::new(*lots).expect("faixa positiva");
            let (order_id, account, seq) = (OrderId::new(id), ACCOUNTS[*account], Seq::new(id));
            let order = if let Some(price) = price {
                let price = Price::new(*price).expect("faixa positiva");
                Order::new(order_id, account, TESTE_BRL, *side, price, lots, seq)
            } else {
                let protection = protection.map(|bound| Price::new(bound).expect("faixa positiva"));
                Order::market(order_id, account, TESTE_BRL, *side, lots, seq, protection)
            };
            Command::Submit(order.expect("ordem válida").with_time_in_force(*tif))
        }
        Op::Cancel { account, target } => Command::Cancel {
            instrument: TESTE_BRL,
            account: ACCOUNTS[*account],
            order: OrderId::new(1 + target.index(usize::try_from(*next_id).unwrap()) as u64),
        },
    }
}

// Borda do colar recalculada aqui, em i128 e sem a função do engine, para o teste não
// confirmar a si mesmo.
fn collar_edge_reference(reference: Price, collar_bps: u32, direction: i128) -> i128 {
    let reference = i128::from(reference.get());
    let band = (reference * i128::from(collar_bps) + 9_999) / 10_000;
    (reference + direction * band).clamp(1, i128::from(i64::MAX))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    // Seção 3.5 pelo Engine inteiro: conservação de valor (soma de cada ativo, contando a
    // conta da casa, = depósitos) e reserva consistente (reservado de cada conta = soma
    // das reservas das suas ordens abertas; reserva existe exatamente para as ordens que
    // estão no livro). E seção 4.1: nenhum fill sai do colar medido antes da ordem.
    #[test]
    fn value_is_conserved_and_reservations_are_consistent(
        taker_bps in prop_oneof![Just(0_u32), Just(1), Just(10), Just(50), Just(9_999), Just(10_000)],
        collar_bps in prop_oneof![Just(0_u32), Just(500), Just(1_000), Just(10_000)],
        max_lots in prop_oneof![Just(3_i64), Just(10)],
        ops in prop::collection::vec(op(), 1..=80)
    ) {
        let fees = FeeSchedule { house: HOUSE, taker_bps: Bps::new(taker_bps).expect("até 100%") };
        let risk = RiskLimits {
            max_lots: Lots::new(max_lots).expect("positivo"),
            collar: Bps::new(collar_bps).expect("até 100%"),
        };
        let listing = ListingConfig { instrument: spec(), risk };
        let mut engine = Engine::new([listing], fees).expect("config válida");
        let mut next_id = 1;
        for op in ops {
            let book = engine.book(TESTE_BRL).expect("instrumento registrado");
            let edge = match &op {
                Op::Submit { side: Side::Bid, .. } => book
                    .best_ask()
                    .map(|reference| (Side::Bid, collar_edge_reference(reference, collar_bps, 1))),
                Op::Submit { side: Side::Ask, .. } => book
                    .best_bid()
                    .map(|reference| (Side::Ask, collar_edge_reference(reference, collar_bps, -1))),
                _ => None,
            };

            let events = engine.apply(command(&op, &mut next_id));

            if let Some((side, edge)) = edge {
                for event in &events {
                    if let Event::Fill { fill, .. } = event {
                        let inside = match side {
                            Side::Bid => i128::from(fill.price.get()) <= edge,
                            Side::Ask => i128::from(fill.price.get()) >= edge,
                        };
                        prop_assert!(inside, "fill {:?} fora do colar {} depois de {:?}", fill, edge, op);
                    }
                }
            }

            for asset in ASSETS {
                let total: i128 = ACCOUNTS
                    .iter()
                    .chain([&HOUSE])
                    .map(|account| engine.balance(*account, asset))
                    .map(|b| i128::from(b.available.get()) + i128::from(b.reserved.get()))
                    .sum();
                prop_assert_eq!(total, i128::from(engine.ledger.supply(asset).get()));
            }

            let mut reserved_by_orders: HashMap<(AccountId, AssetId), i64> = HashMap::new();
            for reservation in engine.reservations.values() {
                *reserved_by_orders
                    .entry((reservation.account, reservation.asset))
                    .or_default() += reservation.amount.get();
            }
            for account in ACCOUNTS {
                for asset in ASSETS {
                    prop_assert_eq!(
                        engine.balance(account, asset).reserved.get(),
                        reserved_by_orders.get(&(account, asset)).copied().unwrap_or_default(),
                        "reservado de {:?}/{:?} diverge das ordens abertas depois de {:?}",
                        account,
                        asset,
                        op
                    );
                }
            }

            let book = engine.book(TESTE_BRL).expect("instrumento registrado");
            let mut resting: Vec<u64> = book.resting_ids().map(OrderId::get).collect();
            let mut reserved: Vec<u64> = engine.reservations.keys().map(|(_, id)| id.get()).collect();
            resting.sort_unstable();
            reserved.sort_unstable();
            prop_assert_eq!(resting, reserved, "reserva sem ordem no livro, ou o contrário");
        }
    }
}
