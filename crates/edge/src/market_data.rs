//! Feed de market data (seção 4.3). [`MarketData`] é o produtor: só consome eventos do
//! núcleo, nunca o engine, e mantém por instrumento um livro agregado por preço.
//! [`LocalBook`] é o assinante: aplica os incrementos sobre um snapshot e detecta buraco
//! pela sequência.
//!
//! Cada instrumento tem dois fluxos com sequências próprias e contíguas, para que quem
//! assina só um não veja buracos falsos: L2 (níveis e negócios) e L1 (topo e negócios).

use std::collections::{BTreeMap, HashMap};

use exchange_core::domain::{InstrumentId, Lots, OrderId, Price, Side};
use exchange_core::engine::Event;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Feed {
    L1,
    L2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Quote {
    pub price: Price,
    pub lots: Lots,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Body {
    /// Quantidade absoluta no nível depois da mudança; zero quer dizer que o nível sumiu.
    /// Absoluta, e não diferença, para que aplicar duas vezes dê o mesmo resultado.
    Level {
        side: Side,
        price: Price,
        lots: Lots,
    },
    Top {
        bid: Option<Quote>,
        ask: Option<Quote>,
    },
    Trade {
        price: Price,
        lots: Lots,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Update {
    pub instrument: InstrumentId,
    pub feed: Feed,
    pub seq: u64,
    pub body: Body,
}

/// O livro de um fluxo no instante `seq`: os incrementos seguintes começam em `seq + 1`.
/// Lados do melhor para o pior; no L1, no máximo um nível de cada.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub instrument: InstrumentId,
    pub feed: Feed,
    pub seq: u64,
    pub bids: Vec<Quote>,
    pub asks: Vec<Quote>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Resting {
    side: Side,
    price: Price,
    lots: Lots,
}

#[derive(Default)]
struct InstrumentFeed {
    bids: BTreeMap<Price, Lots>,
    asks: BTreeMap<Price, Lots>,
    resting: HashMap<OrderId, Resting>,
    l1_seq: u64,
    l2_seq: u64,
}

impl InstrumentFeed {
    fn levels(&mut self, side: Side) -> &mut BTreeMap<Price, Lots> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    fn quotes(&self, side: Side) -> Vec<Quote> {
        let quote = |(&price, &lots): (&Price, &Lots)| Quote { price, lots };
        match side {
            Side::Bid => self.bids.iter().rev().map(quote).collect(),
            Side::Ask => self.asks.iter().map(quote).collect(),
        }
    }

    fn top(&self) -> (Option<Quote>, Option<Quote>) {
        let quote = |(&price, &lots): (&Price, &Lots)| Quote { price, lots };
        (
            self.bids.last_key_value().map(quote),
            self.asks.first_key_value().map(quote),
        )
    }

    /// # Panics
    /// Se o evento não couber no livro do feed — só com evento que o engine não emitiria.
    fn add(&mut self, side: Side, price: Price, lots: Lots) -> Lots {
        let level = self.levels(side).entry(price).or_insert(Lots::ZERO);
        *level = level
            .checked_add(lots)
            .expect("reservas limitam a soma de um nível a i64");
        *level
    }

    /// # Panics
    /// Se tirar mais do que o nível tem — só com evento que o engine não emitiria.
    fn take(&mut self, side: Side, price: Price, lots: Lots) -> Lots {
        let levels = self.levels(side);
        let left = levels
            .get(&price)
            .and_then(|level| level.checked_sub(lots))
            .expect("o feed só tira de nível onde há ordem em repouso");
        if left == Lots::ZERO {
            levels.remove(&price);
        } else {
            levels.insert(price, left);
        }
        left
    }

    fn update(&mut self, instrument: InstrumentId, feed: Feed, body: Body) -> Update {
        let seq = match feed {
            Feed::L1 => &mut self.l1_seq,
            Feed::L2 => &mut self.l2_seq,
        };
        *seq += 1;
        Update {
            instrument,
            feed,
            seq: *seq,
            body,
        }
    }
}

/// O produtor do feed: transforma eventos do núcleo em incrementos numerados.
pub struct MarketData {
    books: HashMap<InstrumentId, InstrumentFeed>,
}

impl MarketData {
    pub fn new(instruments: impl IntoIterator<Item = InstrumentId>) -> Self {
        Self {
            books: instruments
                .into_iter()
                .map(|instrument| (instrument, InstrumentFeed::default()))
                .collect(),
        }
    }

    /// Os incrementos causados pelos eventos de um comando, na ordem em que aconteceram.
    /// O topo (L1) sai no fim, uma vez por instrumento, e só se mudou.
    ///
    /// # Panics
    /// Se os eventos contradisserem o livro do feed (um fill de maker que não está em
    /// repouso, por exemplo). O engine nunca emite isso; se emitir, o invariante quebrou e
    /// parar é o certo (fail-stop).
    pub fn apply(&mut self, events: &[Event]) -> Vec<Update> {
        let mut updates = Vec::new();
        let mut tops_before = Vec::new();
        for event in events {
            let (Event::Rested { instrument, .. }
            | Event::Fill { instrument, .. }
            | Event::Cancelled { instrument, .. }) = *event
            else {
                continue;
            };
            let Some(feed) = self.books.get_mut(&instrument) else {
                continue;
            };
            if !tops_before.iter().any(|&(seen, _)| seen == instrument) {
                tops_before.push((instrument, feed.top()));
            }
            match *event {
                Event::Rested {
                    order,
                    side,
                    price,
                    lots,
                    ..
                } => {
                    feed.resting.insert(order, Resting { side, price, lots });
                    let lots = feed.add(side, price, lots);
                    updates.push(feed.update(
                        instrument,
                        Feed::L2,
                        Body::Level { side, price, lots },
                    ));
                }
                Event::Fill { fill, .. } => {
                    let trade = Body::Trade {
                        price: fill.price,
                        lots: fill.lots,
                    };
                    updates.push(feed.update(instrument, Feed::L2, trade));
                    updates.push(feed.update(instrument, Feed::L1, trade));
                    let maker = feed
                        .resting
                        .get_mut(&fill.maker)
                        .expect("todo maker de um fill está em repouso no feed");
                    let Resting { side, price, .. } = *maker;
                    maker.lots = maker
                        .lots
                        .checked_sub(fill.lots)
                        .expect("um fill nunca passa do restante do maker");
                    if maker.lots == Lots::ZERO {
                        feed.resting.remove(&fill.maker);
                    }
                    let lots = feed.take(side, price, fill.lots);
                    updates.push(feed.update(
                        instrument,
                        Feed::L2,
                        Body::Level { side, price, lots },
                    ));
                }
                Event::Cancelled { order, .. } => {
                    // Sobra de IOC ou de ordem a mercado nunca descansou: nada a tirar.
                    if let Some(Resting { side, price, lots }) = feed.resting.remove(&order) {
                        let lots = feed.take(side, price, lots);
                        updates.push(feed.update(
                            instrument,
                            Feed::L2,
                            Body::Level { side, price, lots },
                        ));
                    }
                }
                _ => {}
            }
        }
        for (instrument, before) in tops_before {
            let feed = self
                .books
                .get_mut(&instrument)
                .expect("só instrumentos registrados entram na lista");
            let (bid, ask) = feed.top();
            if (bid, ask) != before {
                updates.push(feed.update(instrument, Feed::L1, Body::Top { bid, ask }));
            }
        }
        updates
    }

    pub fn snapshot(&self, instrument: InstrumentId, feed: Feed) -> Option<Snapshot> {
        let book = self.books.get(&instrument)?;
        let snapshot = match feed {
            Feed::L2 => Snapshot {
                instrument,
                feed,
                seq: book.l2_seq,
                bids: book.quotes(Side::Bid),
                asks: book.quotes(Side::Ask),
            },
            Feed::L1 => {
                let (bid, ask) = book.top();
                Snapshot {
                    instrument,
                    feed,
                    seq: book.l1_seq,
                    bids: bid.into_iter().collect(),
                    asks: ask.into_iter().collect(),
                }
            }
        };
        Some(snapshot)
    }
}

/// O incremento que chegou não é o próximo: algo se perdeu e a cópia local não é mais
/// confiável. O assinante pede um snapshot novo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gap {
    pub expected: u64,
    pub found: u64,
}

/// A cópia do livro mantida pelo assinante de um fluxo.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LocalBook {
    instrument: InstrumentId,
    feed: Feed,
    seq: u64,
    bids: BTreeMap<Price, Lots>,
    asks: BTreeMap<Price, Lots>,
    last_trade: Option<Quote>,
}

impl LocalBook {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        let levels = |quotes: &[Quote]| {
            quotes
                .iter()
                .map(|quote| (quote.price, quote.lots))
                .collect()
        };
        Self {
            instrument: snapshot.instrument,
            feed: snapshot.feed,
            seq: snapshot.seq,
            bids: levels(&snapshot.bids),
            asks: levels(&snapshot.asks),
            last_trade: None,
        }
    }

    /// Aplica um incremento do fluxo deste livro.
    ///
    /// Incremento de outro instrumento ou fluxo, ou com `seq` já coberto (anterior ou igual
    /// ao do snapshot), é ignorado. Fora isso, só o próximo `seq` é aceito.
    ///
    /// # Errors
    /// [`Gap`] se o `seq` pular o próximo esperado; o livro fica como estava.
    pub fn apply(&mut self, update: &Update) -> Result<(), Gap> {
        if update.instrument != self.instrument
            || update.feed != self.feed
            || update.seq <= self.seq
        {
            return Ok(());
        }
        let expected = self.seq.saturating_add(1);
        if update.seq != expected {
            return Err(Gap {
                expected,
                found: update.seq,
            });
        }
        match update.body {
            Body::Level { side, price, lots } => {
                let levels = match side {
                    Side::Bid => &mut self.bids,
                    Side::Ask => &mut self.asks,
                };
                if lots == Lots::ZERO {
                    levels.remove(&price);
                } else {
                    levels.insert(price, lots);
                }
            }
            Body::Top { bid, ask } => {
                self.bids = bid
                    .map(|quote| (quote.price, quote.lots))
                    .into_iter()
                    .collect();
                self.asks = ask
                    .map(|quote| (quote.price, quote.lots))
                    .into_iter()
                    .collect();
            }
            Body::Trade { price, lots } => self.last_trade = Some(Quote { price, lots }),
        }
        self.seq = update.seq;
        Ok(())
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn last_trade(&self) -> Option<Quote> {
        self.last_trade
    }

    /// O livro local no mesmo formato do snapshot do produtor, para comparar ou exibir.
    pub fn snapshot(&self) -> Snapshot {
        let quote = |(&price, &lots): (&Price, &Lots)| Quote { price, lots };
        Snapshot {
            instrument: self.instrument,
            feed: self.feed,
            seq: self.seq,
            bids: self.bids.iter().rev().map(quote).collect(),
            asks: self.asks.iter().map(quote).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use exchange_core::domain::{AccountId, Amount, OrderKind, TimeInForce};
    use proptest::prelude::*;

    use super::*;
    use crate::market::{BRL, INSTRUMENTS, TESTE};
    use crate::sequencer::{Intent, Sequencer};

    const TESTE_BRL: InstrumentId = INSTRUMENTS[0].1;

    fn price(value: i64) -> Price {
        Price::new(value).unwrap()
    }

    fn lots(value: i64) -> Lots {
        Lots::new(value).unwrap()
    }

    fn quote(price_value: i64, lots_value: i64) -> Quote {
        Quote {
            price: price(price_value),
            lots: lots(lots_value),
        }
    }

    fn fund(account: u64) -> [Intent; 2] {
        let account = AccountId::new(account);
        [
            Intent::Deposit {
                account,
                asset: BRL,
                amount: Amount::new(1_000_000_000).unwrap(),
            },
            Intent::Deposit {
                account,
                asset: TESTE,
                amount: Amount::new(10_000_000).unwrap(),
            },
        ]
    }

    fn order(account: u64, side: Side, kind: OrderKind, quantity: i64, tif: TimeInForce) -> Intent {
        Intent::Order {
            account: AccountId::new(account),
            instrument: TESTE_BRL,
            side,
            kind,
            lots: lots(quantity),
            time_in_force: tif,
        }
    }

    fn limit(account: u64, side: Side, at: i64, quantity: i64) -> Intent {
        order(
            account,
            side,
            OrderKind::Limit(price(at)),
            quantity,
            TimeInForce::Gtc,
        )
    }

    /// Um mercado com o feed ao lado, como vai ser na thread do núcleo.
    struct Market {
        sequencer: Sequencer,
        feed: MarketData,
    }

    impl Market {
        fn new() -> Self {
            let mut market = Self {
                sequencer: Sequencer::in_memory(),
                feed: MarketData::new([TESTE_BRL]),
            };
            for account in 1..=3 {
                for intent in fund(account) {
                    market.submit(intent);
                }
            }
            market
        }

        fn submit(&mut self, intent: Intent) -> Vec<Update> {
            let events = self.sequencer.submit(intent).unwrap();
            self.feed.apply(&events)
        }

        fn snapshot(&self, feed: Feed) -> Snapshot {
            self.feed.snapshot(TESTE_BRL, feed).unwrap()
        }

        fn depth(&self, side: Side) -> Vec<Quote> {
            self.sequencer
                .engine()
                .book(TESTE_BRL)
                .unwrap()
                .depth(side)
                .into_iter()
                .map(|(price, lots)| Quote { price, lots })
                .collect()
        }
    }

    fn update(feed: Feed, seq: u64, body: Body) -> Update {
        Update {
            instrument: TESTE_BRL,
            feed,
            seq,
            body,
        }
    }

    fn level(side: Side, at: i64, quantity: i64) -> Body {
        Body::Level {
            side,
            price: price(at),
            lots: lots(quantity),
        }
    }

    #[test]
    fn deposits_produce_no_updates_and_consume_no_seq() {
        let market = Market::new();

        assert_eq!(market.snapshot(Feed::L1).seq, 0);
        assert_eq!(market.snapshot(Feed::L2).seq, 0);
    }

    #[test]
    fn resting_order_updates_its_level_and_the_top() {
        let mut market = Market::new();

        let updates = market.submit(limit(1, Side::Ask, 10_000, 5));

        assert_eq!(
            updates,
            vec![
                update(Feed::L2, 1, level(Side::Ask, 10_000, 5)),
                update(
                    Feed::L1,
                    1,
                    Body::Top {
                        bid: None,
                        ask: Some(quote(10_000, 5))
                    }
                ),
            ]
        );
    }

    #[test]
    fn order_behind_the_top_changes_only_l2() {
        let mut market = Market::new();
        market.submit(limit(1, Side::Ask, 10_000, 5));

        let updates = market.submit(limit(1, Side::Ask, 10_100, 2));

        assert_eq!(
            updates,
            vec![update(Feed::L2, 2, level(Side::Ask, 10_100, 2))]
        );
    }

    #[test]
    fn fill_reports_the_trade_on_both_feeds_and_shrinks_the_maker_level() {
        let mut market = Market::new();
        market.submit(limit(1, Side::Ask, 10_000, 5));

        let updates = market.submit(limit(2, Side::Bid, 10_000, 2));

        let trade = Body::Trade {
            price: price(10_000),
            lots: lots(2),
        };
        assert_eq!(
            updates,
            vec![
                update(Feed::L2, 2, trade),
                update(Feed::L1, 2, trade),
                update(Feed::L2, 3, level(Side::Ask, 10_000, 3)),
                update(
                    Feed::L1,
                    3,
                    Body::Top {
                        bid: None,
                        ask: Some(quote(10_000, 3))
                    }
                ),
            ]
        );
    }

    #[test]
    fn leftover_that_never_rested_produces_no_level() {
        let mut market = Market::new();
        market.submit(limit(1, Side::Ask, 10_000, 1));

        let updates = market.submit(order(
            2,
            Side::Bid,
            OrderKind::Limit(price(10_000)),
            3,
            TimeInForce::Ioc,
        ));

        assert!(
            updates.iter().all(|update| !matches!(
                update.body,
                Body::Level {
                    side: Side::Bid,
                    ..
                }
            )),
            "{updates:?}"
        );
        assert_eq!(market.snapshot(Feed::L2).bids, vec![]);
    }

    #[test]
    fn cancel_of_resting_order_removes_its_lots() {
        let mut market = Market::new();
        market.submit(limit(1, Side::Bid, 9_900, 4));
        market.submit(limit(2, Side::Bid, 9_900, 1));

        let updates = market.submit(Intent::Cancel {
            account: AccountId::new(1),
            instrument: TESTE_BRL,
            order: OrderId::new(1),
        });

        assert_eq!(
            updates,
            vec![
                update(Feed::L2, 3, level(Side::Bid, 9_900, 1)),
                update(
                    Feed::L1,
                    3,
                    Body::Top {
                        bid: Some(quote(9_900, 1)),
                        ask: None
                    }
                ),
            ]
        );
    }

    #[test]
    fn local_book_ignores_what_the_snapshot_already_covers() {
        let mut market = Market::new();
        let early = market.submit(limit(1, Side::Ask, 10_000, 5));
        let snapshot = market.snapshot(Feed::L2);
        let mut local = LocalBook::from_snapshot(&snapshot);

        for update in &early {
            assert_eq!(local.apply(update), Ok(()));
        }

        assert_eq!(local.snapshot(), snapshot);
    }

    #[test]
    fn local_book_detects_a_gap_and_stays_as_it_was() {
        let mut market = Market::new();
        let mut local = LocalBook::from_snapshot(&market.snapshot(Feed::L2));
        let first = market.submit(limit(1, Side::Ask, 10_000, 5));
        let second = market.submit(limit(1, Side::Ask, 10_100, 2));
        let before = local.clone();

        assert_eq!(
            local.apply(&second[0]),
            Err(Gap {
                expected: 1,
                found: 2
            })
        );
        assert_eq!(local, before);

        for update in first.iter().chain(&second) {
            local.apply(update).unwrap();
        }
        assert_eq!(local.snapshot(), market.snapshot(Feed::L2));
    }

    #[test]
    fn local_book_ignores_other_streams() {
        let mut market = Market::new();
        let mut local = LocalBook::from_snapshot(&market.snapshot(Feed::L1));

        let updates = market.submit(limit(1, Side::Ask, 10_000, 5));
        for update in &updates {
            local.apply(update).unwrap();
        }

        assert_eq!(local.snapshot(), market.snapshot(Feed::L1));
        let foreign = Update {
            instrument: InstrumentId::new(99),
            ..updates[1]
        };
        assert_eq!(local.apply(&foreign), Ok(()));
        assert_eq!(local.snapshot(), market.snapshot(Feed::L1));
    }

    #[test]
    fn local_book_remembers_the_last_trade() {
        let mut market = Market::new();
        market.submit(limit(1, Side::Ask, 10_000, 5));
        let mut local = LocalBook::from_snapshot(&market.snapshot(Feed::L1));

        for update in market.submit(limit(2, Side::Bid, 10_000, 2)) {
            local.apply(&update).unwrap();
        }

        assert_eq!(local.last_trade(), Some(quote(10_000, 2)));
    }

    #[derive(Clone, Debug)]
    enum Op {
        Limit(u64, Side, i64, i64, TimeInForce),
        Market(u64, Side, i64),
        Cancel(u64, u64),
    }

    fn side() -> impl Strategy<Value = Side> {
        prop_oneof![Just(Side::Bid), Just(Side::Ask)]
    }

    // Preços numa faixa estreita para as ordens se cruzarem com frequência, e dentro do
    // colar de 10% para não serem rejeitadas por risco.
    fn op() -> impl Strategy<Value = Op> {
        let account = 1..=3u64;
        prop_oneof![
            6 => (account.clone(), side(), 0..=20i64, 1..=5i64, prop_oneof![
                4 => Just(TimeInForce::Gtc),
                1 => Just(TimeInForce::Ioc),
                1 => Just(TimeInForce::Fok),
            ])
                .prop_map(|(account, side, step, quantity, tif)| {
                    Op::Limit(account, side, 9_900 + 10 * step, quantity, tif)
                }),
            1 => (account.clone(), side(), 1..=5i64)
                .prop_map(|(account, side, quantity)| Op::Market(account, side, quantity)),
            2 => (account, 1..=40u64).prop_map(|(account, order)| Op::Cancel(account, order)),
        ]
    }

    fn intent(op: &Op) -> Intent {
        match *op {
            Op::Limit(account, side, at, quantity, tif) => {
                order(account, side, OrderKind::Limit(price(at)), quantity, tif)
            }
            Op::Market(account, side, quantity) => order(
                account,
                side,
                OrderKind::Market { protection: None },
                quantity,
                TimeInForce::Gtc,
            ),
            Op::Cancel(account, id) => Intent::Cancel {
                account: AccountId::new(account),
                instrument: TESTE_BRL,
                order: OrderId::new(id),
            },
        }
    }

    proptest! {
        // O livro do feed, montado só com eventos, é o livro do engine visto de fora.
        #[test]
        fn feed_book_always_matches_the_engine_depth(ops in prop::collection::vec(op(), 1..60)) {
            let mut market = Market::new();
            for op in &ops {
                market.submit(intent(op));
                let l2 = market.snapshot(Feed::L2);
                prop_assert_eq!(&l2.bids, &market.depth(Side::Bid));
                prop_assert_eq!(&l2.asks, &market.depth(Side::Ask));
                let l1 = market.snapshot(Feed::L1);
                prop_assert_eq!(l1.bids, market.depth(Side::Bid).into_iter().take(1).collect::<Vec<_>>());
                prop_assert_eq!(l1.asks, market.depth(Side::Ask).into_iter().take(1).collect::<Vec<_>>());
            }
        }

        // O assinante que assina antes e pega o snapshot no meio do caminho: recebe também
        // incrementos que o snapshot já cobre, descarta esses e termina igual ao produtor.
        #[test]
        fn snapshot_plus_later_updates_rebuild_the_final_book(
            ops in prop::collection::vec(op(), 1..60),
            cut in any::<prop::sample::Index>(),
        ) {
            let mut market = Market::new();
            let cut = cut.index(ops.len());
            let mut updates = Vec::new();
            let mut snapshots = None;
            for (position, op) in ops.iter().enumerate() {
                if position == cut {
                    snapshots = Some((market.snapshot(Feed::L1), market.snapshot(Feed::L2)));
                }
                updates.extend(market.submit(intent(op)));
            }
            let (l1, l2) = snapshots.expect("o corte cai dentro da sequência");
            let mut local_l1 = LocalBook::from_snapshot(&l1);
            let mut local_l2 = LocalBook::from_snapshot(&l2);
            for update in &updates {
                prop_assert_eq!(local_l1.apply(update), Ok(()));
                prop_assert_eq!(local_l2.apply(update), Ok(()));
            }
            prop_assert_eq!(local_l1.snapshot(), market.snapshot(Feed::L1));
            prop_assert_eq!(local_l2.snapshot(), market.snapshot(Feed::L2));
        }

        #[test]
        fn every_feed_counts_one_by_one(ops in prop::collection::vec(op(), 1..60)) {
            let mut market = Market::new();
            let mut last = HashMap::new();
            for op in &ops {
                for update in market.submit(intent(op)) {
                    let previous = last.insert(update.feed, update.seq).unwrap_or(0);
                    prop_assert_eq!(update.seq, previous + 1, "{:?}", update.feed);
                }
            }
        }
    }
}
