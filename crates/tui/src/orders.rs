//! As ordens da conta como a TUI as conhece, juntadas pelo id: a consulta `orders` traz as
//! abertas no login, e os relatórios contam o que acontece depois.
//!
//! Um relatório nem sempre diz tudo da ordem: o `fill` de uma ordem que a TUI não viu nascer
//! não traz lado nem total. O que não se sabe fica `None` e aparece como "—", em vez de ser
//! adivinhado.

use std::collections::HashMap;

use exchange_protocol::{OpenOrder, OrderState, OrderType, Side, Tif};

/// Quantas ordens terminadas ficam na lista depois das abertas.
pub const KEEP_FINISHED: usize = 20;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum State {
    /// O servidor deu o id pelo `accepted`, e o primeiro relatório ainda não chegou.
    Accepted,
    Resting,
    Armed,
    Filled,
    Cancelled,
    Expired,
    Rejected(String),
    /// Estava aberta e sumiu enquanto a TUI estava desconectada: o servidor só diz que ela
    /// não está mais aberta, não como terminou.
    Closed,
}

impl State {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Accepted | Self::Resting | Self::Armed)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Order {
    pub instrument: String,
    pub id: u64,
    pub side: Option<Side>,
    pub kind: Option<OrderType>,
    /// O limite: de uma ordem limite, de um stop limite ou o preço em que a sobra descansou.
    pub price: Option<i64>,
    pub trigger: Option<i64>,
    pub tif: Option<Tif>,
    pub total: Option<i64>,
    pub filled: i64,
    pub remaining: Option<i64>,
    pub state: State,
    /// Em que ordem terminou, para listar as terminadas da mais recente para a mais antiga.
    finished: Option<u64>,
}

impl Order {
    fn new(instrument: &str, id: u64) -> Self {
        Self {
            instrument: instrument.to_string(),
            id,
            side: None,
            kind: None,
            price: None,
            trigger: None,
            tif: None,
            total: None,
            filled: 0,
            remaining: None,
            state: State::Resting,
            finished: None,
        }
    }
}

/// Uma ordem que a boleta mandou e cujo `accepted` ainda não chegou.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pending {
    pub instrument: String,
    pub side: Side,
    pub kind: OrderType,
    pub price: Option<i64>,
    pub trigger: Option<i64>,
    pub tif: Tif,
    pub lots: i64,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Orders {
    known: Vec<Order>,
    finishes: u64,
    /// Pelo `tag` que foi junto com a ordem e volta no `accepted` com o id dela.
    pending: HashMap<u64, Pending>,
}

impl Orders {
    pub fn get(&self, id: u64) -> Option<&Order> {
        self.known.iter().find(|order| order.id == id)
    }

    pub fn open_count(&self) -> usize {
        self.known
            .iter()
            .filter(|order| order.state.is_open())
            .count()
    }

    pub fn expect(&mut self, tag: u64, pending: Pending) {
        self.pending.insert(tag, pending);
    }

    /// O id da ordem que foi com `tag`: a linha nasce com o que a boleta mandou. Um `tag`
    /// que esta TUI não mandou (de antes de uma reconexão) é ignorado.
    pub fn accepted(&mut self, tag: u64, id: u64) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        let order = self.entry(&pending.instrument, id);
        order.side = Some(pending.side);
        order.kind = Some(pending.kind);
        order.price = pending.price;
        order.trigger = pending.trigger;
        order.tif = Some(pending.tif);
        order.total = Some(pending.lots);
        order.remaining = Some(pending.lots);
        self.reopen(id, State::Accepted);
    }

    /// A ordem que foi com `tag` voltou num `error`: nunca chegou ao núcleo nem tem id.
    pub fn refused(&mut self, tag: u64) {
        self.pending.remove(&tag);
    }

    /// As ordens de um instrumento como a tela lista: abertas primeiro, da mais nova para a
    /// mais antiga, e depois as terminadas, da que terminou por último para a primeira.
    pub fn of(&self, instrument: &str) -> Vec<&Order> {
        let mine = || {
            self.known
                .iter()
                .filter(move |order| order.instrument == instrument)
        };
        let mut open: Vec<&Order> = mine().filter(|order| order.state.is_open()).collect();
        open.sort_by_key(|order| std::cmp::Reverse(order.id));
        let mut done: Vec<&Order> = mine().filter(|order| !order.state.is_open()).collect();
        done.sort_by_key(|order| std::cmp::Reverse(order.finished));
        open.extend(done);
        open
    }

    fn entry(&mut self, instrument: &str, id: u64) -> &mut Order {
        if let Some(index) = self.known.iter().position(|order| order.id == id) {
            return &mut self.known[index];
        }
        self.known.push(Order::new(instrument, id));
        let last = self.known.len() - 1;
        &mut self.known[last]
    }

    fn finish(&mut self, id: u64, state: State) {
        self.finishes += 1;
        let finishes = self.finishes;
        if let Some(order) = self.known.iter_mut().find(|order| order.id == id) {
            order.state = state;
            order.finished = Some(finishes);
        }
        self.trim();
    }

    fn reopen(&mut self, id: u64, state: State) {
        if let Some(order) = self.known.iter_mut().find(|order| order.id == id) {
            order.state = state;
            order.finished = None;
        }
    }

    fn trim(&mut self) {
        let mut finished: Vec<u64> = self.known.iter().filter_map(|o| o.finished).collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort_unstable();
        let cutoff = finished[finished.len() - KEEP_FINISHED];
        self.known
            .retain(|order| order.finished.is_none_or(|finished| finished >= cutoff));
    }

    /// As abertas segundo o servidor, que valem por cima do que a TUI sabia delas.
    /// A resposta do `orders`: as que vêm nela estão abertas, com os números do servidor, e
    /// as que a TUI achava abertas e não vêm terminaram enquanto ela estava fora. Devolve
    /// quantas eram essas.
    pub fn sync(&mut self, open: Vec<OpenOrder>) -> usize {
        let gone: Vec<u64> = self
            .known
            .iter()
            .filter(|order| order.state.is_open())
            .map(|order| order.id)
            .filter(|id| open.iter().all(|open| open.order != *id))
            .collect();
        self.open(open);
        for &id in &gone {
            self.finish(id, State::Closed);
        }
        gone.len()
    }

    pub fn forget_pending(&mut self) {
        self.pending.clear();
    }

    fn open(&mut self, open: Vec<OpenOrder>) {
        for open in open {
            let order = self.entry(&open.instrument, open.order);
            order.side = Some(open.side);
            order.kind = Some(open.kind);
            order.price = open.price;
            order.trigger = open.trigger;
            order.tif = Some(open.tif);
            order.total = Some(open.lots);
            order.filled = open.lots - open.remaining;
            order.remaining = Some(open.remaining);
            let state = match open.state {
                OrderState::Resting => State::Resting,
                OrderState::Armed => State::Armed,
            };
            self.reopen(open.order, state);
        }
    }

    /// A sobra foi para o livro. Uma ordem que só descansa é limite com GTC: IOC e FOK nunca
    /// ficam no livro, e um stop disparado já foi conhecido como stop pelo `armed`.
    pub fn rested(&mut self, instrument: &str, id: u64, side: Side, price: i64, lots: i64) {
        let order = self.entry(instrument, id);
        order.side = Some(side);
        order.price = Some(price);
        order.kind.get_or_insert(OrderType::Limit);
        order.tif.get_or_insert(Tif::Gtc);
        order.remaining = Some(lots);
        order.total.get_or_insert(order.filled + lots);
        self.reopen(id, State::Resting);
    }

    /// Sem saber quanto faltava, a ordem fica como executada: se sobrar algo, o `rested` ou
    /// o `expired` que vem em seguida corrige.
    pub fn filled(&mut self, instrument: &str, id: u64, lots: i64) {
        let order = self.entry(instrument, id);
        order.filled += lots;
        order.remaining = order.remaining.map(|remaining| (remaining - lots).max(0));
        if order.remaining.is_none_or(|remaining| remaining == 0) {
            self.finish(id, State::Filled);
        }
    }

    pub fn cancelled(&mut self, instrument: &str, id: u64) {
        self.entry(instrument, id).remaining = Some(0);
        self.finish(id, State::Cancelled);
    }

    pub fn expired(&mut self, instrument: &str, id: u64) {
        self.entry(instrument, id).remaining = Some(0);
        self.finish(id, State::Expired);
    }

    /// Logo depois do `accepted`, é a própria ordem que foi recusada. Numa ordem que já
    /// estava aberta, é um cancelamento recusado, e ela continua como estava. Uma ordem
    /// que a TUI não conhece (de outra sessão) fica de fora: o `rejected` não diz o
    /// instrumento.
    pub fn rejected(&mut self, id: u64, code: &str) {
        let Some(order) = self.known.iter_mut().find(|order| order.id == id) else {
            return;
        };
        if order.state == State::Accepted {
            order.remaining = Some(0);
            self.finish(id, State::Rejected(code.to_string()));
        }
    }

    pub fn armed(&mut self, instrument: &str, id: u64, side: Side, trigger: i64, lots: i64) {
        let order = self.entry(instrument, id);
        order.side = Some(side);
        order.kind = Some(OrderType::Stop);
        order.trigger = Some(trigger);
        order.total.get_or_insert(lots);
        order.remaining = Some(lots);
        self.reopen(id, State::Armed);
    }

    /// Disparado, o stop vira ordem comum: os eventos seguintes dizem se descansou, executou
    /// ou expirou.
    pub fn triggered(&mut self, instrument: &str, id: u64) {
        self.entry(instrument, id);
        self.reopen(id, State::Resting);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TESTE: &str = "TESTE/BRL";

    fn open(order: u64, state: OrderState) -> OpenOrder {
        OpenOrder {
            instrument: TESTE.to_string(),
            order,
            side: Side::Sell,
            kind: OrderType::Limit,
            price: Some(10_000),
            trigger: None,
            tif: Tif::Gtc,
            lots: 5,
            remaining: 3,
            state,
        }
    }

    fn ids(orders: &Orders) -> Vec<u64> {
        orders.of(TESTE).iter().map(|order| order.id).collect()
    }

    #[test]
    fn open_orders_from_the_server_come_complete() {
        let mut orders = Orders::default();

        orders.sync(vec![open(1, OrderState::Resting)]);

        let order = orders.get(1).unwrap();
        assert_eq!(order.side, Some(Side::Sell));
        assert_eq!(order.total, Some(5));
        assert_eq!(order.filled, 2);
        assert_eq!(order.remaining, Some(3));
        assert_eq!(order.state, State::Resting);
    }

    #[test]
    fn partial_fill_then_rest_keeps_the_order_open_with_its_total() {
        let mut orders = Orders::default();

        orders.filled(TESTE, 7, 2);
        assert_eq!(orders.get(7).unwrap().state, State::Filled);
        orders.rested(TESTE, 7, Side::Buy, 9_900, 3);

        let order = orders.get(7).unwrap();
        assert_eq!(order.state, State::Resting);
        assert_eq!(order.total, Some(5));
        assert_eq!(order.filled, 2);
        assert_eq!(order.remaining, Some(3));
        assert_eq!(order.kind, Some(OrderType::Limit));
    }

    #[test]
    fn fills_on_a_resting_order_finish_it_when_nothing_remains() {
        let mut orders = Orders::default();
        orders.rested(TESTE, 1, Side::Sell, 10_000, 5);

        orders.filled(TESTE, 1, 2);
        assert_eq!(orders.get(1).unwrap().state, State::Resting);
        orders.filled(TESTE, 1, 3);

        let order = orders.get(1).unwrap();
        assert_eq!(order.state, State::Filled);
        assert_eq!(order.filled, 5);
        assert_eq!(order.remaining, Some(0));
    }

    #[test]
    fn cancel_rejection_leaves_the_open_order_alive() {
        let mut orders = Orders::default();
        orders.armed(TESTE, 2, Side::Sell, 9_500, 2);

        orders.rejected(2, "stop_already_triggered");

        assert_eq!(orders.get(2).unwrap().state, State::Armed);
    }

    fn pending(side: Side, kind: OrderType, tif: Tif) -> Pending {
        Pending {
            instrument: TESTE.to_string(),
            side,
            kind,
            price: Some(10_000),
            trigger: None,
            tif,
            lots: 5,
        }
    }

    #[test]
    fn accepted_order_starts_with_what_the_ticket_sent() {
        let mut orders = Orders::default();
        orders.expect(1, pending(Side::Buy, OrderType::Market, Tif::Ioc));

        orders.accepted(1, 4);
        assert_eq!(orders.get(4).unwrap().state, State::Accepted);
        orders.filled(TESTE, 4, 5);

        let order = orders.get(4).unwrap();
        assert_eq!(order.kind, Some(OrderType::Market));
        assert_eq!(order.tif, Some(Tif::Ioc));
        assert_eq!(order.side, Some(Side::Buy));
        assert_eq!(order.total, Some(5));
        assert_eq!(order.state, State::Filled);
    }

    #[test]
    fn tags_pair_each_order_with_its_id_whatever_the_order_of_arrival() {
        let mut orders = Orders::default();
        orders.expect(1, pending(Side::Buy, OrderType::Limit, Tif::Gtc));
        orders.expect(2, pending(Side::Sell, OrderType::Market, Tif::Fok));

        orders.rested(TESTE, 3, Side::Buy, 9_000, 1);
        orders.accepted(2, 5);
        orders.accepted(1, 6);

        assert_eq!(orders.get(3).unwrap().total, Some(1));
        assert_eq!(orders.get(5).unwrap().kind, Some(OrderType::Market));
        assert_eq!(orders.get(6).unwrap().kind, Some(OrderType::Limit));
    }

    #[test]
    fn rejection_right_after_acceptance_finishes_the_order() {
        let mut orders = Orders::default();
        orders.expect(1, pending(Side::Buy, OrderType::Limit, Tif::Gtc));
        orders.accepted(1, 9);

        orders.rejected(9, "insufficient_funds");

        let order = orders.get(9).unwrap();
        assert_eq!(
            order.state,
            State::Rejected("insufficient_funds".to_string())
        );
        assert_eq!(order.instrument, TESTE);
        assert_eq!(orders.open_count(), 0);
    }

    #[test]
    fn rejection_of_an_unknown_order_is_not_listed() {
        let mut orders = Orders::default();

        orders.rejected(9, "insufficient_funds");

        assert!(orders.get(9).is_none());
    }

    #[test]
    fn refused_or_unknown_tags_leave_no_order_behind() {
        let mut orders = Orders::default();
        orders.expect(1, pending(Side::Buy, OrderType::Limit, Tif::Gtc));

        orders.refused(1);
        orders.accepted(1, 4);
        orders.accepted(77, 5);

        assert!(orders.get(4).is_none());
        assert!(orders.get(5).is_none());
    }

    #[test]
    fn open_count_ignores_finished_orders() {
        let mut orders = Orders::default();
        orders.rested(TESTE, 1, Side::Sell, 10_000, 5);
        orders.armed(TESTE, 2, Side::Sell, 9_500, 2);
        orders.cancelled(TESTE, 1);

        assert_eq!(orders.open_count(), 1);
    }

    #[test]
    fn triggered_stop_follows_its_events() {
        let mut orders = Orders::default();
        orders.armed(TESTE, 3, Side::Buy, 10_100, 4);

        orders.triggered(TESTE, 3);
        orders.filled(TESTE, 3, 1);
        orders.expired(TESTE, 3);

        let order = orders.get(3).unwrap();
        assert_eq!(order.kind, Some(OrderType::Stop));
        assert_eq!(order.filled, 1);
        assert_eq!(order.state, State::Expired);
    }

    #[test]
    fn open_first_newest_first_then_finished_latest_first() {
        let mut orders = Orders::default();
        orders.rested(TESTE, 1, Side::Sell, 10_000, 5);
        orders.rested(TESTE, 2, Side::Sell, 10_100, 5);
        orders.rested(TESTE, 3, Side::Sell, 10_200, 5);
        orders.rested(TESTE, 4, Side::Sell, 10_300, 5);
        orders.cancelled(TESTE, 3);
        orders.cancelled(TESTE, 1);
        orders.rested("OUTRO/BRL", 5, Side::Buy, 500, 1);

        assert_eq!(ids(&orders), [4, 2, 1, 3]);
    }

    #[test]
    fn only_the_latest_finished_are_kept() {
        let mut orders = Orders::default();
        orders.rested(TESTE, 1_000, Side::Buy, 9_000, 1);
        let extra = u64::try_from(KEEP_FINISHED).unwrap() + 5;
        for id in 1..=extra {
            orders.cancelled(TESTE, id);
        }

        let listed = ids(&orders);
        assert_eq!(listed.len(), KEEP_FINISHED + 1);
        assert_eq!(listed[0], 1_000);
        assert_eq!(listed[1], extra);
        assert!(orders.get(1).is_none());
    }
}
