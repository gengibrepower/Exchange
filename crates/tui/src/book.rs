//! O livro de um fluxo do feed (seção 4.3), montado pelo snapshot e mantido pelos
//! incrementos, sobre os números crus do fio.

use std::collections::BTreeMap;

use exchange_protocol::{Level, Side};

/// Faltou um incremento: o livro não corresponde mais ao do servidor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gap {
    pub expected: u64,
    pub found: u64,
}

/// O que fazer com um incremento, conforme o `seq` dele.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sequence {
    /// É o próximo do fluxo: aplicar.
    Next,
    /// Já coberto pelo snapshot ou repetido: ignorar.
    Stale,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Book {
    seq: u64,
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
}

impl Book {
    /// O livro no instante `seq`: o próximo incremento esperado é `seq + 1`.
    pub fn from_snapshot(seq: u64, bids: &[Level], asks: &[Level]) -> Self {
        let levels = |levels: &[Level]| {
            levels
                .iter()
                .map(|level| (level.price, level.lots))
                .collect()
        };
        Self {
            seq,
            bids: levels(bids),
            asks: levels(asks),
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Confere o `seq` de um incremento e, se for o próximo, avança o livro para ele.
    ///
    /// # Errors
    /// [`Gap`] se o `seq` pular algum: o livro deve ser descartado e reassinado.
    pub fn advance(&mut self, seq: u64) -> Result<Sequence, Gap> {
        if seq <= self.seq {
            return Ok(Sequence::Stale);
        }
        let expected = self.seq + 1;
        if seq != expected {
            return Err(Gap {
                expected,
                found: seq,
            });
        }
        self.seq = seq;
        Ok(Sequence::Next)
    }

    /// Nível 2: a quantidade absoluta no preço; zero tira o nível.
    pub fn set_level(&mut self, side: Side, price: i64, lots: i64) {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        if lots == 0 {
            levels.remove(&price);
        } else {
            levels.insert(price, lots);
        }
    }

    /// Nível 1: o topo inteiro é trocado; `None` esvazia o lado.
    pub fn set_top(&mut self, bid: Option<Level>, ask: Option<Level>) {
        let only = |level: Option<Level>| {
            level
                .map(|level| (level.price, level.lots))
                .into_iter()
                .collect()
        };
        self.bids = only(bid);
        self.asks = only(ask);
    }

    /// Do maior preço para o menor: o melhor primeiro.
    pub fn bids(&self) -> impl DoubleEndedIterator<Item = Level> + '_ {
        self.bids.iter().rev().map(level)
    }

    /// Do menor preço para o maior: o melhor primeiro.
    pub fn asks(&self) -> impl DoubleEndedIterator<Item = Level> + '_ {
        self.asks.iter().map(level)
    }

    pub fn best_bid(&self) -> Option<Level> {
        self.bids().next()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.asks().next()
    }
}

fn level((&price, &lots): (&i64, &i64)) -> Level {
    Level { price, lots }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(price: i64, lots: i64) -> Level {
        Level { price, lots }
    }

    fn book() -> Book {
        Book::from_snapshot(
            4,
            &[at(9_800, 1), at(9_900, 3)],
            &[at(10_100, 2), at(10_000, 5)],
        )
    }

    #[test]
    fn sides_come_best_first() {
        let book = book();

        assert_eq!(
            book.bids().collect::<Vec<_>>(),
            [at(9_900, 3), at(9_800, 1)]
        );
        assert_eq!(
            book.asks().collect::<Vec<_>>(),
            [at(10_000, 5), at(10_100, 2)]
        );
        assert_eq!(book.best_bid(), Some(at(9_900, 3)));
        assert_eq!(book.best_ask(), Some(at(10_000, 5)));
    }

    #[test]
    fn only_the_next_seq_advances() {
        let mut book = book();

        assert_eq!(book.advance(3), Ok(Sequence::Stale));
        assert_eq!(book.advance(4), Ok(Sequence::Stale));
        assert_eq!(book.advance(5), Ok(Sequence::Next));
        assert_eq!(book.seq(), 5);
        assert_eq!(
            book.advance(7),
            Err(Gap {
                expected: 6,
                found: 7
            })
        );
        assert_eq!(book.seq(), 5);
    }

    #[test]
    fn level_is_absolute_and_zero_removes_it() {
        let mut book = book();

        book.set_level(Side::Sell, 10_000, 2);
        book.set_level(Side::Buy, 9_900, 0);
        book.set_level(Side::Buy, 9_950, 4);

        assert_eq!(book.best_ask(), Some(at(10_000, 2)));
        assert_eq!(
            book.bids().collect::<Vec<_>>(),
            [at(9_950, 4), at(9_800, 1)]
        );
    }

    #[test]
    fn top_replaces_both_sides() {
        let mut book = book();

        book.set_top(Some(at(9_950, 4)), None);

        assert_eq!(book.bids().collect::<Vec<_>>(), [at(9_950, 4)]);
        assert_eq!(book.best_ask(), None);
    }
}
