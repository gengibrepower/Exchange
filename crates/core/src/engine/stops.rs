use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use crate::book::CancelError;
use crate::domain::{AccountId, Order, OrderId, Price, Seq, Side};

/// Stops dormentes de um instrumento: fora do livro e invisíveis ao feed. A chave de cada
/// lado já é a ordem de disparo: o gatilho mais perto do preço primeiro (compras em
/// gatilho crescente, vendas em decrescente) e, no empate, quem chegou antes.
#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct StopBook {
    buys: BTreeMap<(Price, Seq), Order>,
    sells: BTreeMap<(Reverse<Price>, Seq), Order>,
    index: HashMap<OrderId, (Side, Price, Seq)>,
}

impl StopBook {
    pub(super) fn contains(&self, id: OrderId) -> bool {
        self.index.contains_key(&id)
    }

    pub(super) fn arm(&mut self, order: Order, trigger: Price) {
        let (side, seq) = (order.side(), order.seq());
        self.index.insert(order.id(), (side, trigger, seq));
        match side {
            Side::Bid => self.buys.insert((trigger, seq), order),
            Side::Ask => self.sells.insert((Reverse(trigger), seq), order),
        };
    }

    /// # Errors
    /// [`CancelError::UnknownOrder`] se não houver stop dormente com esse id;
    /// [`CancelError::NotOwner`] se ele for de outra conta. Em ambos, nada muda.
    pub(super) fn cancel(&mut self, id: OrderId, account: AccountId) -> Result<Order, CancelError> {
        let &(side, trigger, seq) = self.index.get(&id).ok_or(CancelError::UnknownOrder)?;
        let owner = match side {
            Side::Bid => self.buys.get(&(trigger, seq)),
            Side::Ask => self.sells.get(&(Reverse(trigger), seq)),
        }
        .map(Order::account);
        if owner != Some(account) {
            return Err(CancelError::NotOwner);
        }
        self.remove(id).ok_or(CancelError::UnknownOrder)
    }

    /// O próximo stop a disparar com o último negócio em `last_trade`, já fora do livro de
    /// stops, com o seu gatilho. Se os dois lados tiverem stop elegível (o preço saltou
    /// por cima de ambos), sai o que chegou antes.
    pub(super) fn pop_triggered(&mut self, last_trade: Price) -> Option<(Order, Price)> {
        let buy_seq = self
            .buys
            .first_key_value()
            .filter(|((trigger, _), _)| last_trade >= *trigger)
            .map(|(&(_, seq), _)| seq);
        let sell_seq = self
            .sells
            .first_key_value()
            .filter(|((Reverse(trigger), _), _)| last_trade <= *trigger)
            .map(|(&(_, seq), _)| seq);
        let side = match (buy_seq, sell_seq) {
            (Some(bid), Some(ask)) if ask < bid => Side::Ask,
            (Some(_), _) => Side::Bid,
            (None, Some(_)) => Side::Ask,
            (None, None) => return None,
        };
        let ((trigger, _), order) = match side {
            Side::Bid => self.buys.pop_first()?,
            Side::Ask => self
                .sells
                .pop_first()
                .map(|((Reverse(trigger), seq), order)| ((trigger, seq), order))?,
        };
        self.index.remove(&order.id());
        Some((order, trigger))
    }

    pub(super) fn orders(&self) -> impl Iterator<Item = &Order> {
        self.buys.values().chain(self.sells.values())
    }

    #[cfg(test)]
    pub(super) fn ids(&self) -> impl Iterator<Item = OrderId> + '_ {
        self.index.keys().copied()
    }

    fn remove(&mut self, id: OrderId) -> Option<Order> {
        let (side, trigger, seq) = self.index.remove(&id)?;
        match side {
            Side::Bid => self.buys.remove(&(trigger, seq)),
            Side::Ask => self.sells.remove(&(Reverse(trigger), seq)),
        }
    }
}
