// Modelo de referência do livro: propositalmente ingênuo (uma Vec, busca linear a cada
// fill) para ser conferível a olho. Serve de oráculo nos proptests; por isso não reusa
// a lógica de cruzamento nem de prioridade do OrderBook — um bug lá não pode se repetir aqui.

use super::{CancelError, Leftover, MatchOutcome};
use crate::domain::{AccountId, Fill, Lots, Order, OrderId, OrderStatus, Side, TimeInForce};
use std::cmp::Reverse;

#[derive(Default)]
pub(super) struct ReferenceBook {
    resting: Vec<Order>,
}

impl ReferenceBook {
    pub(super) fn submit(&mut self, mut taker: Order) -> MatchOutcome {
        if taker.time_in_force() == TimeInForce::Fok {
            let available: i64 = self
                .resting
                .iter()
                .filter(|maker| is_eligible(&taker, maker))
                .map(|maker| maker.remaining().get())
                .sum();
            if available < taker.remaining().get() {
                let lots = taker.remaining();
                taker
                    .transition(OrderStatus::Expired)
                    .expect("FOK aceita e sem fill pode expirar");
                return MatchOutcome {
                    fills: Vec::new(),
                    leftover: Leftover::Expired(lots),
                };
            }
        }

        let mut fills = Vec::new();
        while taker.remaining() > Lots::ZERO {
            let Some(index) = self.best_maker(&taker) else {
                break;
            };
            let maker = &mut self.resting[index];
            let traded = taker.remaining().min(maker.remaining());
            fills.push(Fill {
                taker: taker.id(),
                maker: maker.id(),
                price: maker.limit_price().expect("só ordem limite descansa"),
                lots: traded,
            });
            taker.fill(traded).expect("traded dentro do restante");
            maker.fill(traded).expect("traded dentro do restante");
            if maker.remaining() == Lots::ZERO {
                self.resting.remove(index);
            }
        }

        let remaining = taker.remaining();
        let leftover = if remaining == Lots::ZERO {
            Leftover::None
        } else if taker.time_in_force() == TimeInForce::Gtc && taker.limit_price().is_some() {
            self.resting.push(taker);
            Leftover::Rested(remaining)
        } else {
            taker
                .transition(OrderStatus::Cancelled)
                .expect("sobra pode ser cancelada");
            Leftover::Cancelled(remaining)
        };
        MatchOutcome { fills, leftover }
    }

    pub(super) fn cancel(&mut self, id: OrderId, account: AccountId) -> Result<Order, CancelError> {
        let Some(index) = self.resting.iter().position(|order| order.id() == id) else {
            return Err(CancelError::UnknownOrder);
        };
        if self.resting[index].account() != account {
            return Err(CancelError::NotOwner);
        }
        let mut order = self.resting.remove(index);
        order
            .transition(OrderStatus::Cancelled)
            .expect("ordem descansando pode ser cancelada");
        Ok(order)
    }

    pub(super) fn resting(&self) -> &[Order] {
        &self.resting
    }

    /// Índice, em `resting`, do maker que o taker deve atingir agora: entre os elegíveis,
    /// o de melhor preço para o taker; no empate, o de menor `Seq` (chegou primeiro).
    fn best_maker(&self, taker: &Order) -> Option<usize> {
        let eligible = self
            .resting
            .iter()
            .enumerate()
            .filter(|(_, maker)| is_eligible(taker, maker));
        match taker.side() {
            Side::Bid => eligible
                .min_by_key(|(_, maker)| (maker.limit_price(), maker.seq()))
                .map(|(index, _)| index),
            Side::Ask => eligible
                .min_by_key(|(_, maker)| (Reverse(maker.limit_price()), maker.seq()))
                .map(|(index, _)| index),
        }
    }
}

fn is_eligible(taker: &Order, maker: &Order) -> bool {
    let maker_price = maker.limit_price().expect("só ordem limite descansa");
    let opposite = taker.side() != maker.side();
    let price_ok = match taker.price_bound() {
        None => true,
        Some(bound) => match taker.side() {
            Side::Bid => maker_price.get() <= bound.get(),
            Side::Ask => maker_price.get() >= bound.get(),
        },
    };
    opposite && price_ok
}
