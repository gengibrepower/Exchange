//! As ordens da conta como a TUI as conhece, juntadas pelo id: a consulta `orders` traz as
//! abertas no login, e os relatórios contam o que acontece depois.
//!
//! Um relatório nem sempre diz tudo da ordem: o `fill` de uma ordem que a TUI não viu nascer
//! não traz lado nem total. O que não se sabe fica `None` e aparece como "—", em vez de ser
//! adivinhado.

use exchange_protocol::{OpenOrder, OrderState, OrderType, Side, Tif};

/// Quantas ordens terminadas ficam na lista depois das abertas.
pub const KEEP_FINISHED: usize = 20;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum State {
    Resting,
    Armed,
    Filled,
    Cancelled,
    Expired,
    Rejected(String),
}

impl State {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Resting | Self::Armed)
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

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Orders {
    orders: Vec<Order>,
    finishes: u64,
}

impl Orders {
    pub fn get(&self, id: u64) -> Option<&Order> {
        self.orders.iter().find(|order| order.id == id)
    }

    /// As ordens de um instrumento como a tela lista: abertas primeiro, da mais nova para a
    /// mais antiga, e depois as terminadas, da que terminou por último para a primeira.
    pub fn of(&self, instrument: &str) -> Vec<&Order> {
        let mine = || {
            self.orders
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
        if let Some(index) = self.orders.iter().position(|order| order.id == id) {
            return &mut self.orders[index];
        }
        self.orders.push(Order::new(instrument, id));
        let last = self.orders.len() - 1;
        &mut self.orders[last]
    }

    fn finish(&mut self, id: u64, state: State) {
        self.finishes += 1;
        let finishes = self.finishes;
        if let Some(order) = self.orders.iter_mut().find(|order| order.id == id) {
            order.state = state;
            order.finished = Some(finishes);
        }
        self.trim();
    }

    fn reopen(&mut self, id: u64, state: State) {
        if let Some(order) = self.orders.iter_mut().find(|order| order.id == id) {
            order.state = state;
            order.finished = None;
        }
    }

    fn trim(&mut self) {
        let mut finished: Vec<u64> = self.orders.iter().filter_map(|o| o.finished).collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort_unstable();
        let cutoff = finished[finished.len() - KEEP_FINISHED];
        self.orders
            .retain(|order| order.finished.is_none_or(|finished| finished >= cutoff));
    }

    /// As abertas segundo o servidor, que valem por cima do que a TUI sabia delas.
    pub fn open(&mut self, open: Vec<OpenOrder>) {
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

    /// Uma recusa com o id de uma ordem conhecida é de um cancelamento: a ordem continua
    /// como estava. Com id novo, é a própria ordem que foi recusada; como o `rejected` não
    /// diz o instrumento, só quem sabe para onde a ordem ia (`instrument`) a registra.
    pub fn rejected(&mut self, instrument: Option<&str>, id: u64, code: &str) {
        if self.get(id).is_some() {
            return;
        }
        if let Some(instrument) = instrument {
            self.entry(instrument, id);
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

        orders.open(vec![open(1, OrderState::Resting)]);

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

        orders.rejected(Some(TESTE), 2, "stop_already_triggered");

        assert_eq!(orders.get(2).unwrap().state, State::Armed);
    }

    #[test]
    fn rejection_of_a_new_order_is_kept_as_finished() {
        let mut orders = Orders::default();

        orders.rejected(Some(TESTE), 9, "insufficient_funds");

        assert_eq!(
            orders.get(9).unwrap().state,
            State::Rejected("insufficient_funds".to_string())
        );
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
