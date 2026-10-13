use std::collections::HashMap;

use proptest::prelude::*;

use super::{Ledger, Transfer};
use crate::domain::{AccountId, Amount, AssetId};

const ACCOUNTS: u64 = 3;
const ASSETS: u32 = 2;

#[derive(Clone, Debug)]
enum Op {
    Deposit(AccountId, AssetId, Amount),
    Reserve(AccountId, AssetId, Amount),
    Release(AccountId, AssetId, Amount),
    Settle(Vec<Transfer>),
}

fn account() -> impl Strategy<Value = AccountId> {
    (1..=ACCOUNTS).prop_map(AccountId::new)
}

fn asset() -> impl Strategy<Value = AssetId> {
    (1..=ASSETS).prop_map(AssetId::new)
}

// Valores pequenos e poucas contas de propósito: assim reservas e liquidações batem no
// limite do saldo com frequência, e os caminhos de erro são exercitados tanto quanto os
// de sucesso.
fn amount(max: i64) -> impl Strategy<Value = Amount> {
    (0..=max).prop_map(|value| Amount::new(value).expect("faixa não negativa"))
}

fn transfer() -> impl Strategy<Value = Transfer> {
    (asset(), account(), account(), amount(60)).prop_map(|(asset, from, to, amount)| Transfer {
        asset,
        from,
        to,
        amount,
    })
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (account(), asset(), amount(200)).prop_map(|(a, s, v)| Op::Deposit(a, s, v)),
        (account(), asset(), amount(100)).prop_map(|(a, s, v)| Op::Reserve(a, s, v)),
        (account(), asset(), amount(100)).prop_map(|(a, s, v)| Op::Release(a, s, v)),
        prop::collection::vec(transfer(), 0..=3).prop_map(Op::Settle),
    ]
}

fn total(ledger: &Ledger, asset: AssetId) -> i128 {
    (1..=ACCOUNTS)
        .map(AccountId::new)
        .map(|account| ledger.balance(account, asset))
        .map(|balance| i128::from(balance.available.get()) + i128::from(balance.reserved.get()))
        .sum()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    // Seção 3.5, conservação de valor: só depósito muda a soma de um ativo; reserva,
    // liberação e liquidação apenas movem valor. E operação que falha não muda nada.
    #[test]
    fn value_is_conserved_and_failures_change_nothing(ops in prop::collection::vec(op(), 1..=60)) {
        let mut ledger = Ledger::new();
        let mut deposited: HashMap<AssetId, i128> = HashMap::new();
        for op in ops {
            let before = ledger.clone();
            let result = match &op {
                Op::Deposit(account, asset, amount) => ledger.deposit(*account, *asset, *amount),
                Op::Reserve(account, asset, amount) => ledger.reserve(*account, *asset, *amount),
                Op::Release(account, asset, amount) => ledger.release(*account, *asset, *amount),
                Op::Settle(transfers) => ledger.settle(transfers),
            };
            match (&op, &result) {
                (Op::Deposit(_, asset, amount), Ok(())) => {
                    *deposited.entry(*asset).or_default() += i128::from(amount.get());
                }
                (_, Err(_)) => prop_assert_eq!(&ledger, &before, "{:?} falhou e mudou o ledger", op),
                _ => {}
            }
            for asset in (1..=ASSETS).map(AssetId::new) {
                prop_assert_eq!(
                    total(&ledger, asset),
                    deposited.get(&asset).copied().unwrap_or_default(),
                    "soma de {:?} mudou depois de {:?}",
                    asset,
                    op
                );
            }
        }
    }
}
