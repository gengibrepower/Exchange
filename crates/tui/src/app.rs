//! O estado da TUI e como ele muda. Sem terminal e sem rede: as mensagens entram por
//! [`App::update`], e o que precisa sair (uma mensagem ao servidor, o fim do programa) volta
//! como [`Effect`] para o laço principal executar.

use exchange_protocol::{ClientMessage, Feed, InstrumentSpec, Level, ServerMessage, Side};

use crate::book::{Book, Gap, Sequence};

/// Níveis por lado que o livro mostra e que o cursor alcança.
pub const DEPTH: usize = 10;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    Server(ServerMessage),
    Key(Key),
    /// O terminal mudou de tamanho: nada muda no estado, mas vale redesenhar.
    Resize,
    Disconnected(String),
}

/// As teclas que a TUI entende, já separadas do terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    /// Ctrl-C: no modo cru do terminal ele chega como tecla, não como sinal.
    Interrupt,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    Send(ClientMessage),
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Market,
    Instrument(usize),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Connection {
    Connecting,
    Online,
    Offline(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Info,
    Ok,
    Error,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Status {
    pub text: String,
    pub tone: Tone,
}

/// Para onde andou o preço em relação ao negócio anterior: o `trade` do feed não diz quem
/// agrediu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tick {
    Up,
    Down,
    Flat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trade {
    pub price: i64,
    pub lots: i64,
    pub tick: Tick,
}

/// Uma linha do livro como a tela mostra: vendas em cima, do pior preço para o melhor, e
/// compras embaixo, do melhor para o pior.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub side: Side,
    pub level: Level,
}

/// O nível sob o cursor, guardado pelo preço: o cursor fica no lugar quando outros níveis
/// aparecem ou somem.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub side: Side,
    pub price: i64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Market {
    pub spec: InstrumentSpec,
    /// Fluxo L1: o topo e o último negócio, para a aba Mercado.
    pub top: Option<Book>,
    /// Fluxo L2: a profundidade, para a aba do instrumento.
    pub depth: Option<Book>,
    pub last: Option<Trade>,
    pub selected: Option<Selection>,
}

impl Market {
    fn new(spec: InstrumentSpec) -> Self {
        Self {
            spec,
            top: None,
            depth: None,
            last: None,
            selected: None,
        }
    }

    fn stream(&mut self, feed: Feed) -> &mut Option<Book> {
        match feed {
            Feed::L1 => &mut self.top,
            Feed::L2 => &mut self.depth,
        }
    }

    /// Se o incremento `seq` é para aplicar: só o próximo do fluxo, e só com livro montado.
    /// Num buraco o livro é descartado, e os incrementos seguintes são ignorados até chegar
    /// um snapshot novo.
    fn sequenced(&mut self, feed: Feed, seq: u64) -> Result<bool, Gap> {
        let stream = self.stream(feed);
        let Some(book) = stream else {
            return Ok(false);
        };
        match book.advance(seq) {
            Ok(sequence) => Ok(sequence == Sequence::Next),
            Err(gap) => {
                *stream = None;
                Err(gap)
            }
        }
    }

    fn record_trade(&mut self, price: i64, lots: i64) {
        let tick = match self.last {
            Some(last) if price > last.price => Tick::Up,
            Some(last) if price < last.price => Tick::Down,
            _ => Tick::Flat,
        };
        self.last = Some(Trade { price, lots, tick });
    }

    /// As linhas do livro L2 que a tela mostra, até [`DEPTH`] por lado.
    pub fn ladder(&self) -> Vec<Row> {
        let Some(book) = &self.depth else {
            return Vec::new();
        };
        let asks: Vec<Row> = book
            .asks()
            .take(DEPTH)
            .map(|level| Row {
                side: Side::Sell,
                level,
            })
            .collect();
        let bids = book.bids().take(DEPTH).map(|level| Row {
            side: Side::Buy,
            level,
        });
        asks.into_iter().rev().chain(bids).collect()
    }

    pub fn is_selected(&self, row: &Row) -> bool {
        self.selected == Some(selection(row))
    }

    /// Anda uma linha para baixo (`down`) ou para cima. Sem seleção, ou se o nível sumiu,
    /// começa do lado do spread para onde se andou: `j` cai na melhor compra, `k` na melhor
    /// venda.
    fn move_selection(&mut self, down: bool) {
        let ladder = self.ladder();
        if ladder.is_empty() {
            self.selected = None;
            return;
        }
        let current = ladder.iter().position(|row| self.is_selected(row));
        let asks = ladder.iter().filter(|row| row.side == Side::Sell).count();
        let last = ladder.len() - 1;
        let index = match (current, down) {
            (Some(index), true) => (index + 1).min(last),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => asks.min(last),
            (None, false) => asks.saturating_sub(1),
        };
        self.selected = Some(selection(&ladder[index]));
    }
}

fn selection(row: &Row) -> Selection {
    Selection {
        side: row.side,
        price: row.level.price,
    }
}

pub struct App {
    pub server: String,
    pub account: u64,
    pub connection: Connection,
    pub markets: Vec<Market>,
    pub tab: Tab,
    pub market_cursor: usize,
    pub status: Option<Status>,
    pub help: bool,
}

impl App {
    pub fn new(server: String, account: u64) -> Self {
        Self {
            server,
            account,
            connection: Connection::Connecting,
            markets: Vec::new(),
            tab: Tab::Market,
            market_cursor: 0,
            status: None,
            help: false,
        }
    }

    /// O que mandar logo depois de conectar: o login e o pedido dos instrumentos, cuja
    /// resposta dispara as assinaturas.
    pub fn start(&self) -> Vec<Effect> {
        vec![
            Effect::Send(ClientMessage::Login {
                account: self.account,
            }),
            Effect::Send(ClientMessage::Instruments {}),
        ]
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Server(message) => self.on_server(message),
            Msg::Key(key) => self.on_key(key),
            Msg::Resize => Vec::new(),
            Msg::Disconnected(reason) => {
                self.say(Tone::Error, format!("conexão perdida: {reason}"));
                self.connection = Connection::Offline(reason);
                Vec::new()
            }
        }
    }

    /// O mercado da aba aberta; `None` na aba Mercado.
    pub fn current(&self) -> Option<&Market> {
        match self.tab {
            Tab::Market => None,
            Tab::Instrument(index) => self.markets.get(index),
        }
    }

    fn say(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            tone,
        });
    }

    fn market(&mut self, symbol: &str) -> Option<&mut Market> {
        self.markets
            .iter_mut()
            .find(|market| market.spec.symbol == symbol)
    }

    fn on_server(&mut self, message: ServerMessage) -> Vec<Effect> {
        match message {
            ServerMessage::LoggedIn { account } => {
                self.connection = Connection::Online;
                self.say(Tone::Ok, format!("conta {account} conectada"));
                Vec::new()
            }
            ServerMessage::Instruments { instruments } => self.on_instruments(instruments),
            ServerMessage::Error { message } => {
                self.say(Tone::Error, message);
                Vec::new()
            }
            ServerMessage::Snapshot {
                instrument,
                feed,
                seq,
                bids,
                asks,
            } => {
                if let Some(market) = self.market(&instrument) {
                    *market.stream(feed) = Some(Book::from_snapshot(seq, &bids, &asks));
                }
                Vec::new()
            }
            ServerMessage::Depth {
                instrument,
                feed,
                seq,
                side,
                price,
                lots,
            } => self.on_increment(&instrument, feed, seq, |market| {
                if let Some(book) = market.stream(feed) {
                    book.set_level(side, price, lots);
                }
            }),
            ServerMessage::Top {
                instrument,
                feed,
                seq,
                bid,
                ask,
            } => self.on_increment(&instrument, feed, seq, |market| {
                if let Some(book) = market.stream(feed) {
                    book.set_top(bid, ask);
                }
            }),
            // Todo negócio sai nos dois fluxos; o último preço vem só do L1 para não contar
            // o mesmo negócio duas vezes.
            ServerMessage::Trade {
                instrument,
                feed,
                seq,
                price,
                lots,
            } => self.on_increment(&instrument, feed, seq, |market| {
                if feed == Feed::L1 {
                    market.record_trade(price, lots);
                }
            }),
            // Os relatórios da conta e os saldos chegam à tela nas próximas versões.
            ServerMessage::Deposited { .. }
            | ServerMessage::DepositRejected { .. }
            | ServerMessage::Fill { .. }
            | ServerMessage::Rested { .. }
            | ServerMessage::Cancelled { .. }
            | ServerMessage::Expired { .. }
            | ServerMessage::Rejected { .. }
            | ServerMessage::Armed { .. }
            | ServerMessage::Triggered { .. }
            | ServerMessage::Balances { .. } => Vec::new(),
        }
    }

    fn on_instruments(&mut self, instruments: Vec<InstrumentSpec>) -> Vec<Effect> {
        let subscriptions = instruments
            .iter()
            .flat_map(|spec| {
                [Feed::L1, Feed::L2].map(|feed| {
                    Effect::Send(ClientMessage::Subscribe {
                        instrument: spec.symbol.clone(),
                        feed,
                    })
                })
            })
            .collect();
        self.markets = instruments.into_iter().map(Market::new).collect();
        self.tab = Tab::Market;
        self.market_cursor = 0;
        subscriptions
    }

    fn on_increment(
        &mut self,
        instrument: &str,
        feed: Feed,
        seq: u64,
        apply: impl FnOnce(&mut Market),
    ) -> Vec<Effect> {
        let Some(market) = self.market(instrument) else {
            return Vec::new();
        };
        match market.sequenced(feed, seq) {
            Ok(true) => {
                apply(market);
                Vec::new()
            }
            Ok(false) => Vec::new(),
            Err(gap) => {
                self.say(
                    Tone::Info,
                    format!(
                        "buraco no feed de {instrument} (esperava {}, veio {}); pedindo snapshot novo",
                        gap.expected, gap.found
                    ),
                );
                vec![Effect::Send(ClientMessage::Subscribe {
                    instrument: instrument.to_string(),
                    feed,
                })]
            }
        }
    }

    fn on_key(&mut self, key: Key) -> Vec<Effect> {
        if matches!(key, Key::Interrupt | Key::Char('q')) {
            return vec![Effect::Quit];
        }
        if self.help {
            if matches!(key, Key::Char('?') | Key::Esc) {
                self.help = false;
            }
            return Vec::new();
        }
        match key {
            Key::Char('?') => self.help = true,
            Key::Char(']') => self.tab = self.next_tab(true),
            Key::Char('[') => self.tab = self.next_tab(false),
            Key::Char('j') | Key::Down => self.move_cursor(true),
            Key::Char('k') | Key::Up => self.move_cursor(false),
            Key::Enter if self.tab == Tab::Market && self.market_cursor < self.markets.len() => {
                self.tab = Tab::Instrument(self.market_cursor);
            }
            _ => {}
        }
        Vec::new()
    }

    /// A aba seguinte (`forward`) ou a anterior, dando a volta: Mercado, depois um
    /// instrumento por aba.
    fn next_tab(&self, forward: bool) -> Tab {
        let count = self.markets.len() + 1;
        let index = match self.tab {
            Tab::Market => 0,
            Tab::Instrument(index) => index + 1,
        };
        let index = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        match index {
            0 => Tab::Market,
            index => Tab::Instrument(index - 1),
        }
    }

    fn move_cursor(&mut self, down: bool) {
        match self.tab {
            Tab::Market => {
                let last = self.markets.len().saturating_sub(1);
                self.market_cursor = if down {
                    (self.market_cursor + 1).min(last)
                } else {
                    self.market_cursor.saturating_sub(1)
                };
            }
            Tab::Instrument(index) => {
                if let Some(market) = self.markets.get_mut(index) {
                    market.move_selection(down);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(symbol: &str, tick: i64) -> InstrumentSpec {
        InstrumentSpec {
            symbol: symbol.to_string(),
            base: symbol.split('/').next().unwrap().to_string(),
            quote: "BRL".to_string(),
            tick,
            lot_size: 100,
            max_lots: 1_000,
        }
    }

    fn at(price: i64, lots: i64) -> Level {
        Level { price, lots }
    }

    fn server(app: &mut App, message: ServerMessage) -> Vec<Effect> {
        app.update(Msg::Server(message))
    }

    fn press(app: &mut App, keys: &str) {
        for key in keys.chars() {
            app.update(Msg::Key(Key::Char(key)));
        }
    }

    /// Conectado, com dois instrumentos e o L2 de TESTE/BRL montado no seq 4.
    fn ready() -> App {
        let mut app = App::new("127.0.0.1:7000".to_string(), 1);
        server(&mut app, ServerMessage::LoggedIn { account: 1 });
        server(
            &mut app,
            ServerMessage::Instruments {
                instruments: vec![spec("TESTE/BRL", 10), spec("OUTRO/BRL", 5)],
            },
        );
        server(&mut app, snapshot(Feed::L2, 4));
        app
    }

    fn snapshot(feed: Feed, seq: u64) -> ServerMessage {
        ServerMessage::Snapshot {
            instrument: "TESTE/BRL".to_string(),
            feed,
            seq,
            bids: vec![at(9_900, 3), at(9_800, 1)],
            asks: vec![at(10_000, 5), at(10_100, 2)],
        }
    }

    fn depth(seq: u64, side: Side, price: i64, lots: i64) -> ServerMessage {
        ServerMessage::Depth {
            instrument: "TESTE/BRL".to_string(),
            feed: Feed::L2,
            seq,
            side,
            price,
            lots,
        }
    }

    fn trade(feed: Feed, seq: u64, price: i64) -> ServerMessage {
        ServerMessage::Trade {
            instrument: "TESTE/BRL".to_string(),
            feed,
            seq,
            price,
            lots: 1,
        }
    }

    fn teste(app: &App) -> &Market {
        &app.markets[0]
    }

    #[test]
    fn start_logs_in_and_asks_for_the_instruments() {
        let app = App::new("x".to_string(), 7);

        assert_eq!(
            app.start(),
            [
                Effect::Send(ClientMessage::Login { account: 7 }),
                Effect::Send(ClientMessage::Instruments {}),
            ]
        );
    }

    #[test]
    fn instruments_open_both_feeds_of_each() {
        let mut app = App::new("x".to_string(), 1);

        let effects = server(
            &mut app,
            ServerMessage::Instruments {
                instruments: vec![spec("TESTE/BRL", 10), spec("OUTRO/BRL", 5)],
            },
        );

        let subscribe = |instrument: &str, feed| {
            Effect::Send(ClientMessage::Subscribe {
                instrument: instrument.to_string(),
                feed,
            })
        };
        assert_eq!(
            effects,
            [
                subscribe("TESTE/BRL", Feed::L1),
                subscribe("TESTE/BRL", Feed::L2),
                subscribe("OUTRO/BRL", Feed::L1),
                subscribe("OUTRO/BRL", Feed::L2),
            ]
        );
        assert_eq!(app.markets.len(), 2);
    }

    #[test]
    fn login_puts_the_session_online() {
        let app = ready();

        assert_eq!(app.connection, Connection::Online);
        assert_eq!(app.status.as_ref().unwrap().tone, Tone::Ok);
    }

    #[test]
    fn depth_moves_the_l2_book() {
        let mut app = ready();

        assert_eq!(server(&mut app, depth(5, Side::Sell, 10_000, 2)), []);
        server(&mut app, depth(6, Side::Buy, 9_900, 0));

        let book = teste(&app).depth.as_ref().unwrap();
        assert_eq!(book.best_ask(), Some(at(10_000, 2)));
        assert_eq!(book.best_bid(), Some(at(9_800, 1)));
        assert_eq!(book.seq(), 6);
    }

    #[test]
    fn increments_already_covered_by_the_snapshot_change_nothing() {
        let mut app = ready();

        server(&mut app, depth(4, Side::Sell, 10_000, 99));

        let book = teste(&app).depth.as_ref().unwrap();
        assert_eq!(book.best_ask(), Some(at(10_000, 5)));
    }

    #[test]
    fn gap_drops_the_book_and_resubscribes_that_feed() {
        let mut app = ready();

        let effects = server(&mut app, depth(6, Side::Sell, 10_000, 2));

        assert_eq!(
            effects,
            [Effect::Send(ClientMessage::Subscribe {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2,
            })]
        );
        assert!(teste(&app).depth.is_none());
        assert_eq!(server(&mut app, depth(7, Side::Sell, 10_000, 1)), []);

        server(&mut app, snapshot(Feed::L2, 7));
        assert_eq!(teste(&app).depth.as_ref().unwrap().seq(), 7);
    }

    #[test]
    fn top_replaces_the_l1_book() {
        let mut app = ready();
        server(&mut app, snapshot(Feed::L1, 0));

        server(
            &mut app,
            ServerMessage::Top {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L1,
                seq: 1,
                bid: Some(at(9_950, 4)),
                ask: None,
            },
        );

        let top = teste(&app).top.as_ref().unwrap();
        assert_eq!(top.best_bid(), Some(at(9_950, 4)));
        assert_eq!(top.best_ask(), None);
    }

    #[test]
    fn last_trade_comes_from_l1_and_ticks_against_the_previous() {
        let mut app = ready();
        server(&mut app, snapshot(Feed::L1, 0));

        server(&mut app, trade(Feed::L1, 1, 10_000));
        assert_eq!(teste(&app).last.unwrap().tick, Tick::Flat);
        server(&mut app, trade(Feed::L1, 2, 10_100));
        assert_eq!(teste(&app).last.unwrap().tick, Tick::Up);
        server(&mut app, trade(Feed::L1, 3, 9_900));
        assert_eq!(teste(&app).last.unwrap().tick, Tick::Down);

        server(&mut app, trade(Feed::L2, 5, 5_000));
        assert_eq!(teste(&app).last.unwrap().price, 9_900);
        assert_eq!(teste(&app).depth.as_ref().unwrap().seq(), 5);
    }

    #[test]
    fn server_error_and_disconnection_reach_the_status() {
        let mut app = ready();

        server(
            &mut app,
            ServerMessage::Error {
                message: "a conta da casa não negocia".to_string(),
            },
        );
        assert_eq!(app.status.as_ref().unwrap().tone, Tone::Error);

        app.update(Msg::Disconnected("o servidor fechou a conexão".to_string()));
        assert_eq!(
            app.connection,
            Connection::Offline("o servidor fechou a conexão".to_string())
        );
    }

    #[test]
    fn brackets_cycle_through_the_tabs() {
        let mut app = ready();

        press(&mut app, "]");
        assert_eq!(app.tab, Tab::Instrument(0));
        press(&mut app, "]]");
        assert_eq!(app.tab, Tab::Market);
        press(&mut app, "[");
        assert_eq!(app.tab, Tab::Instrument(1));
    }

    #[test]
    fn enter_opens_the_instrument_under_the_cursor() {
        let mut app = ready();

        press(&mut app, "jjj");
        assert_eq!(app.market_cursor, 1);
        app.update(Msg::Key(Key::Enter));
        assert_eq!(app.tab, Tab::Instrument(1));
        assert_eq!(app.current().unwrap().spec.symbol, "OUTRO/BRL");
    }

    #[test]
    fn ladder_shows_asks_worst_first_then_bids_best_first() {
        let app = ready();

        let prices: Vec<i64> = teste(&app)
            .ladder()
            .iter()
            .map(|row| row.level.price)
            .collect();
        assert_eq!(prices, [10_100, 10_000, 9_900, 9_800]);
    }

    #[test]
    fn book_cursor_starts_at_the_spread_and_stops_at_the_edges() {
        let mut app = ready();
        press(&mut app, "]");

        press(&mut app, "j");
        assert_eq!(
            teste(&app).selected,
            Some(Selection {
                side: Side::Buy,
                price: 9_900
            })
        );
        press(&mut app, "jjj");
        assert_eq!(teste(&app).selected.unwrap().price, 9_800);
        press(&mut app, "kkkkk");
        assert_eq!(
            teste(&app).selected,
            Some(Selection {
                side: Side::Sell,
                price: 10_100
            })
        );
    }

    #[test]
    fn cursor_without_selection_going_up_lands_on_the_best_ask() {
        let mut app = ready();
        press(&mut app, "]");

        app.update(Msg::Key(Key::Up));

        assert_eq!(teste(&app).selected.unwrap().price, 10_000);
    }

    #[test]
    fn cursor_follows_the_price_when_levels_change() {
        let mut app = ready();
        press(&mut app, "]j");

        server(&mut app, depth(5, Side::Buy, 9_950, 7));

        assert_eq!(teste(&app).selected.unwrap().price, 9_900);
        press(&mut app, "k");
        assert_eq!(teste(&app).selected.unwrap().price, 9_950);
    }

    #[test]
    fn help_swallows_keys_until_closed() {
        let mut app = ready();

        press(&mut app, "?]");
        assert!(app.help);
        assert_eq!(app.tab, Tab::Market);
        app.update(Msg::Key(Key::Esc));
        assert!(!app.help);
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut app = ready();

        assert_eq!(app.update(Msg::Key(Key::Char('q'))), [Effect::Quit]);
        assert_eq!(app.update(Msg::Key(Key::Interrupt)), [Effect::Quit]);
    }
}
