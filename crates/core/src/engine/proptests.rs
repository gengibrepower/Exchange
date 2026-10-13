use std::collections::HashMap;

use proptest::prelude::*;

use super::{Command, Engine};
use crate::domain::{
    AccountId, Amount, AssetId, Instrument, InstrumentId, Lots, Order, OrderId, Price, Seq, Side,
    TimeInForce,
};

const TESTE_BRL: InstrumentId = InstrumentId::new(1);
const TESTE: AssetId = AssetId::new(1);
const BRL: AssetId = AssetId::new(2);
const ACCOUNTS: [AccountId; 2] = [AccountId::new(1), AccountId::new(2)];
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
    prop_oneof![
        2 => (0..2_usize, prop_oneof![Just(TESTE), Just(BRL)], 0..=600_i64)
            .prop_map(|(account, asset, value)| Op::Deposit(account, asset, value)),
        6 => (0..2_usize, side, price, 1..=10_i64, tif).prop_map(
            |(account, side, price, lots, tif)| Op::Submit { account, side, price, lots, tif }
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
            lots,
            tif,
        } => {
            let id = *next_id;
            *next_id += 1;
            let lots = Lots::new(*lots).expect("faixa positiva");
            let (order_id, account, seq) = (OrderId::new(id), ACCOUNTS[*account], Seq::new(id));
            let order = match price {
                Some(price) => {
                    let price = Price::new(*price).expect("faixa positiva");
                    Order::new(order_id, account, TESTE_BRL, *side, price, lots, seq)
                }
                None => Order::market(order_id, account, TESTE_BRL, *side, lots, seq),
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    // Seção 3.5 pelo Engine inteiro: conservação de valor (soma de cada ativo = depósitos)
    // e reserva consistente (reservado de cada conta = soma das reservas das suas ordens
    // abertas; reserva existe exatamente para as ordens que estão no livro).
    #[test]
    fn value_is_conserved_and_reservations_are_consistent(
        ops in prop::collection::vec(op(), 1..=80)
    ) {
        let mut engine = Engine::new([spec()]).expect("config válida");
        let mut next_id = 1;
        for op in ops {
            engine.apply(command(&op, &mut next_id));

            for asset in ASSETS {
                let total: i128 = ACCOUNTS
                    .iter()
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
