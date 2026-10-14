use proptest::prelude::*;

use super::LogRecord;
use crate::domain::{
    AccountId, Amount, AssetId, InstrumentId, Lots, OrderId, OrderKind, OrderRequest, Price, Seq,
    Side, TimeInForce,
};
use crate::engine::Command;

// Faixas inteiras de propósito: o postcard grava inteiros em tamanho variável, e os
// extremos (0, 1, i64::MAX) passam por todos os comprimentos de varint.
fn price() -> impl Strategy<Value = Price> {
    (1..=i64::MAX).prop_map(|units| Price::new(units).expect("faixa positiva"))
}

fn kind() -> impl Strategy<Value = OrderKind> {
    prop_oneof![
        price().prop_map(OrderKind::Limit),
        proptest::option::of(price()).prop_map(|protection| OrderKind::Market { protection }),
    ]
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

fn time_in_force() -> impl Strategy<Value = TimeInForce> {
    prop_oneof![
        Just(TimeInForce::Gtc),
        Just(TimeInForce::Ioc),
        Just(TimeInForce::Fok),
    ]
}

fn request() -> impl Strategy<Value = OrderRequest> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
        side(),
        kind(),
        0..=i64::MAX,
        any::<u64>(),
        time_in_force(),
    )
        .prop_map(
            |(id, account, instrument, side, kind, total, seq, time_in_force)| OrderRequest {
                id: OrderId::new(id),
                account: AccountId::new(account),
                instrument: InstrumentId::new(instrument),
                side,
                kind,
                total: Lots::new(total).expect("faixa não negativa"),
                seq: Seq::new(seq),
                time_in_force,
            },
        )
}

fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        (any::<u64>(), any::<u32>(), 0..=i64::MAX).prop_map(|(account, asset, amount)| {
            Command::Deposit {
                account: AccountId::new(account),
                asset: AssetId::new(asset),
                amount: Amount::new(amount).expect("faixa não negativa"),
            }
        }),
        request().prop_map(Command::Submit),
        (any::<u32>(), any::<u64>(), any::<u64>()).prop_map(|(instrument, account, order)| {
            Command::Cancel {
                instrument: InstrumentId::new(instrument),
                account: AccountId::new(account),
                order: OrderId::new(order),
            }
        }),
    ]
}

proptest! {
    #[test]
    fn roundtrip(seq in any::<u64>(), command in command()) {
        let record = LogRecord::new(Seq::new(seq), command);
        prop_assert_eq!(LogRecord::from_bytes(&record.to_bytes()), Ok(record));
    }
}
