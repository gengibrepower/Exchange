use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::domain::{AccountId, Amount, AssetId};

/// Seção 3.5, não-negatividade: garantida pelo tipo. `Amount` só nasce de
/// `Amount::new`, que recusa negativo, ou de aritmética `checked_*` que devolve `None`
/// em vez de ficar abaixo de zero — não há como um saldo negativo existir.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Balance {
    pub available: Amount,
    pub reserved: Amount,
}

/// Partida dobrada completa: debita `amount` do `reserved` de `from` e credita o mesmo
/// `amount` no `available` de `to`, no mesmo ativo. Não existe transferência desbalanceada.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transfer {
    pub asset: AssetId,
    pub from: AccountId,
    pub to: AccountId,
    pub amount: Amount,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedgerError {
    InsufficientAvailable {
        account: AccountId,
        asset: AssetId,
        available: Amount,
        requested: Amount,
    },
    InsufficientReserved {
        account: AccountId,
        asset: AssetId,
        reserved: Amount,
        requested: Amount,
    },
    Overflow {
        account: AccountId,
        asset: AssetId,
    },
    SupplyOverflow {
        asset: AssetId,
    },
}

/// Dono único dos saldos (decisão 10b). Toda operação é atômica: em erro, nada muda.
///
/// A oferta total de cada ativo nunca passa de `i64::MAX`. Como nenhum saldo é maior que
/// a soma de todos, nenhum crédito pode estourar — o que deixa a liquidação infalível
/// quando a reserva cobre os débitos.
#[derive(Default, PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(Clone))]
pub struct Ledger {
    balances: HashMap<(AccountId, AssetId), Balance>,
    supply: HashMap<AssetId, Amount>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn balance(&self, account: AccountId, asset: AssetId) -> Balance {
        self.balances
            .get(&(account, asset))
            .copied()
            .unwrap_or_default()
    }

    /// Todo saldo já tocado, inclusive os que voltaram a zero, em ordem de conta e ativo.
    pub fn balances(&self) -> Vec<(AccountId, AssetId, Balance)> {
        let mut balances: Vec<_> = self
            .balances
            .iter()
            .map(|(&(account, asset), &balance)| (account, asset, balance))
            .collect();
        balances.sort_by_key(|&(account, asset, _)| (account.get(), asset.get()));
        balances
    }

    /// Um ledger com estes saldos. A oferta de cada ativo não vem de fora: é a soma dos
    /// saldos, o que torna a conservação verdadeira por construção.
    ///
    /// # Errors
    /// O ativo cuja soma passa de `i64::MAX`.
    pub fn restore(
        balances: impl IntoIterator<Item = (AccountId, AssetId, Balance)>,
    ) -> Result<Self, AssetId> {
        let mut ledger = Self::new();
        for (account, asset, balance) in balances {
            let supply = ledger
                .supply(asset)
                .checked_add(balance.available)
                .and_then(|supply| supply.checked_add(balance.reserved))
                .ok_or(asset)?;
            ledger.supply.insert(asset, supply);
            ledger.balances.insert((account, asset), balance);
        }
        Ok(ledger)
    }

    pub fn supply(&self, asset: AssetId) -> Amount {
        self.supply.get(&asset).copied().unwrap_or_default()
    }

    /// Entrada de valor de fora do sistema: único jeito de a soma de um ativo crescer.
    ///
    /// # Errors
    /// [`LedgerError::SupplyOverflow`] se a oferta total do ativo passar de `i64::MAX`.
    pub fn deposit(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    ) -> Result<(), LedgerError> {
        let supply = self
            .supply(asset)
            .checked_add(amount)
            .ok_or(LedgerError::SupplyOverflow { asset })?;
        let balance = self.balance(account, asset);
        let available = balance
            .available
            .checked_add(amount)
            .ok_or(LedgerError::Overflow { account, asset })?;
        self.supply.insert(asset, supply);
        self.balances.insert(
            (account, asset),
            Balance {
                available,
                ..balance
            },
        );
        Ok(())
    }

    /// # Errors
    /// [`LedgerError::InsufficientAvailable`] se não houver disponível para travar,
    /// [`LedgerError::Overflow`] se o reservado estourar.
    pub fn reserve(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    ) -> Result<(), LedgerError> {
        let balance = self.balance(account, asset);
        let available =
            balance
                .available
                .checked_sub(amount)
                .ok_or(LedgerError::InsufficientAvailable {
                    account,
                    asset,
                    available: balance.available,
                    requested: amount,
                })?;
        let reserved = balance
            .reserved
            .checked_add(amount)
            .ok_or(LedgerError::Overflow { account, asset })?;
        self.balances.insert(
            (account, asset),
            Balance {
                available,
                reserved,
            },
        );
        Ok(())
    }

    /// # Errors
    /// [`LedgerError::InsufficientReserved`] se não houver reservado para liberar,
    /// [`LedgerError::Overflow`] se o disponível estourar.
    pub fn release(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: Amount,
    ) -> Result<(), LedgerError> {
        let balance = self.balance(account, asset);
        let reserved =
            balance
                .reserved
                .checked_sub(amount)
                .ok_or(LedgerError::InsufficientReserved {
                    account,
                    asset,
                    reserved: balance.reserved,
                    requested: amount,
                })?;
        let available = balance
            .available
            .checked_add(amount)
            .ok_or(LedgerError::Overflow { account, asset })?;
        self.balances.insert(
            (account, asset),
            Balance {
                available,
                reserved,
            },
        );
        Ok(())
    }

    /// Aplica todas as transferências ou nenhuma. Transferências que tocam o mesmo saldo
    /// se acumulam: cada uma é conferida contra o saldo já alterado pelas anteriores.
    ///
    /// # Errors
    /// O da primeira transferência que não couber, com os valores daquele momento:
    /// [`LedgerError::InsufficientReserved`] no débito, [`LedgerError::Overflow`] no crédito.
    pub fn settle(&mut self, transfers: &[Transfer]) -> Result<(), LedgerError> {
        let mut staged: HashMap<(AccountId, AssetId), Balance> = HashMap::new();
        for transfer in transfers {
            let debit_key = (transfer.from, transfer.asset);
            let from = self.staged_balance(&staged, debit_key);
            let reserved = from.reserved.checked_sub(transfer.amount).ok_or(
                LedgerError::InsufficientReserved {
                    account: transfer.from,
                    asset: transfer.asset,
                    reserved: from.reserved,
                    requested: transfer.amount,
                },
            )?;
            staged.insert(debit_key, Balance { reserved, ..from });
            let credit_key = (transfer.to, transfer.asset);
            let to = self.staged_balance(&staged, credit_key);
            let available =
                to.available
                    .checked_add(transfer.amount)
                    .ok_or(LedgerError::Overflow {
                        account: transfer.to,
                        asset: transfer.asset,
                    })?;
            staged.insert(credit_key, Balance { available, ..to });
        }
        self.balances.extend(staged);
        Ok(())
    }

    fn staged_balance(
        &self,
        staged: &HashMap<(AccountId, AssetId), Balance>,
        key: (AccountId, AssetId),
    ) -> Balance {
        staged
            .get(&key)
            .copied()
            .unwrap_or_else(|| self.balance(key.0, key.1))
    }
}

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;

    const B: AccountId = AccountId::new(1);
    const S: AccountId = AccountId::new(2);
    const H: AccountId = AccountId::new(99);
    const TESTE: AssetId = AssetId::new(1);
    const BRL: AssetId = AssetId::new(2);

    fn amount(value: i64) -> Amount {
        Amount::new(value).expect("quantia de teste válida")
    }

    fn balance(available: i64, reserved: i64) -> Balance {
        Balance {
            available: amount(available),
            reserved: amount(reserved),
        }
    }

    fn transfer(asset: AssetId, from: AccountId, to: AccountId, value: i64) -> Transfer {
        Transfer {
            asset,
            from,
            to,
            amount: amount(value),
        }
    }

    #[test]
    fn unknown_account_has_zero_balance() {
        assert_eq!(Ledger::new().balance(B, BRL), Balance::default());
    }

    #[test]
    fn deposit_adds_to_available() {
        let mut ledger = Ledger::new();

        ledger.deposit(B, BRL, amount(100)).unwrap();
        ledger.deposit(B, BRL, amount(50)).unwrap();

        assert_eq!(ledger.balance(B, BRL), balance(150, 0));
        assert_eq!(ledger.balance(B, TESTE), Balance::default());
    }

    #[test]
    fn deposit_beyond_total_supply_changes_nothing() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(i64::MAX)).unwrap();
        let before = ledger.clone();

        let result = ledger.deposit(S, BRL, amount(1));

        assert_eq!(result, Err(LedgerError::SupplyOverflow { asset: BRL }));
        assert_eq!(ledger, before);
        assert_eq!(ledger.supply(BRL), amount(i64::MAX));
    }

    #[test]
    fn supply_is_per_asset() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(i64::MAX)).unwrap();

        ledger.deposit(B, TESTE, amount(i64::MAX)).unwrap();

        assert_eq!(ledger.supply(TESTE), amount(i64::MAX));
    }

    #[test]
    fn reserve_and_release_move_between_buckets() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(100)).unwrap();

        ledger.reserve(B, BRL, amount(70)).unwrap();
        assert_eq!(ledger.balance(B, BRL), balance(30, 70));

        ledger.release(B, BRL, amount(20)).unwrap();
        assert_eq!(ledger.balance(B, BRL), balance(50, 50));
    }

    #[test]
    fn reserve_beyond_available_changes_nothing() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(100)).unwrap();
        let before = ledger.clone();

        let result = ledger.reserve(B, BRL, amount(101));

        assert_eq!(
            result,
            Err(LedgerError::InsufficientAvailable {
                account: B,
                asset: BRL,
                available: amount(100),
                requested: amount(101),
            })
        );
        assert_eq!(ledger, before);
    }

    #[test]
    fn release_beyond_reserved_changes_nothing() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(100)).unwrap();
        ledger.reserve(B, BRL, amount(10)).unwrap();
        let before = ledger.clone();

        let result = ledger.release(B, BRL, amount(11));

        assert!(matches!(
            result,
            Err(LedgerError::InsufficientReserved { .. })
        ));
        assert_eq!(ledger, before);
    }

    #[test]
    fn settle_moves_reserved_of_one_account_to_available_of_another() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(300)).unwrap();
        ledger.reserve(B, BRL, amount(200)).unwrap();

        ledger.settle(&[transfer(BRL, B, S, 200)]).unwrap();

        assert_eq!(ledger.balance(B, BRL), balance(100, 0));
        assert_eq!(ledger.balance(S, BRL), balance(200, 0));
    }

    // O exemplo da seção 4.2: B leva 2 lots @ 100 de S, taxa de taker 1 para a casa H.
    #[test]
    fn settle_applies_the_section_4_2_fill_with_fee() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(201)).unwrap();
        ledger.reserve(B, BRL, amount(201)).unwrap();
        ledger.deposit(S, TESTE, amount(2)).unwrap();
        ledger.reserve(S, TESTE, amount(2)).unwrap();

        ledger
            .settle(&[
                transfer(TESTE, S, B, 2),
                transfer(BRL, B, S, 200),
                transfer(BRL, B, H, 1),
            ])
            .unwrap();

        assert_eq!(ledger.balance(B, TESTE), balance(2, 0));
        assert_eq!(ledger.balance(B, BRL), balance(0, 0));
        assert_eq!(ledger.balance(S, TESTE), balance(0, 0));
        assert_eq!(ledger.balance(S, BRL), balance(200, 0));
        assert_eq!(ledger.balance(H, BRL), balance(1, 0));
    }

    #[test]
    fn settle_is_all_or_nothing_when_transfers_share_a_balance() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(250)).unwrap();
        ledger.reserve(B, BRL, amount(250)).unwrap();
        let before = ledger.clone();

        let result = ledger.settle(&[transfer(BRL, B, S, 200), transfer(BRL, B, H, 100)]);

        assert_eq!(
            result,
            Err(LedgerError::InsufficientReserved {
                account: B,
                asset: BRL,
                reserved: amount(50),
                requested: amount(100),
            })
        );
        assert_eq!(ledger, before);
    }

    #[test]
    fn settle_to_the_same_account_releases_into_available() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(10)).unwrap();
        ledger.reserve(B, BRL, amount(10)).unwrap();

        ledger.settle(&[transfer(BRL, B, B, 10)]).unwrap();

        assert_eq!(ledger.balance(B, BRL), balance(10, 0));
    }

    #[test]
    fn settle_of_nothing_changes_nothing() {
        let mut ledger = Ledger::new();
        ledger.deposit(B, BRL, amount(10)).unwrap();
        let before = ledger.clone();

        ledger.settle(&[]).unwrap();

        assert_eq!(ledger, before);
    }
}
