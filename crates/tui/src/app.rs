//! O estado da TUI e como ele muda. Sem terminal e sem rede: as mensagens entram por
//! [`App::update`], e o que precisa sair (uma mensagem ao servidor, o fim do programa) volta
//! como [`Effect`] para o laço principal executar.

use std::collections::VecDeque;

use exchange_protocol::{
    AssetBalance, ClientMessage, Feed, InstrumentSpec, Level, Role, ServerMessage, Side,
};

use crate::book::{Book, Gap, Sequence};
use crate::format::{brl, grouped};
use crate::orders::Orders;
use crate::ticket::Ticket;

/// Níveis por lado que o livro mostra e que o cursor alcança.
pub const DEPTH: usize = 10;

/// Negócios guardados por instrumento no painel Negócios.
pub const TAPE: usize = 100;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// `at` é a hora de chegada na TUI, como texto: o `trade` do feed não traz hora, e o
    /// relógio fica do lado de fora do `update`, que continua determinístico.
    Server {
        message: ServerMessage,
        at: String,
    },
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
pub enum Focus {
    Book,
    Orders,
    Ticket,
}

impl Focus {
    /// A ordem do `Tab`: livro, ordens, boleta, e de volta ao livro.
    fn next(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Book, true) | (Self::Ticket, false) => Self::Orders,
            (Self::Orders, true) | (Self::Book, false) => Self::Ticket,
            (Self::Ticket, true) | (Self::Orders, false) => Self::Book,
        }
    }
}

/// Uma pergunta de sim ou não na linha de status: só `y` confirma, e qualquer outra tecla
/// desiste sem fazer mais nada.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Prompt {
    Cancel { instrument: String, order: u64 },
    Quit { open: usize },
}

impl Prompt {
    pub fn question(&self) -> String {
        match self {
            Self::Cancel { order, .. } => format!("cancelar #{order}? y/n"),
            Self::Quit { open: 1 } => "há 1 ordem aberta; sair mesmo? y/n".to_string(),
            Self::Quit { open } => format!("há {open} ordens abertas; sair mesmo? y/n"),
        }
    }
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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Print {
    pub at: String,
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
    /// Os negócios do fluxo L2, do mais recente para o mais antigo.
    pub tape: VecDeque<Print>,
    pub selected: Option<Selection>,
    /// Uma boleta por instrumento, que guarda o que foi usado por último nele.
    pub ticket: Ticket,
}

impl Market {
    fn new(spec: InstrumentSpec) -> Self {
        Self {
            spec,
            top: None,
            depth: None,
            last: None,
            tape: VecDeque::new(),
            selected: None,
            ticket: Ticket::default(),
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

    fn print(&mut self, at: &str, price: i64, lots: i64) {
        let tick = match self.tape.front() {
            Some(last) if price > last.price => Tick::Up,
            Some(last) if price < last.price => Tick::Down,
            _ => Tick::Flat,
        };
        self.tape.push_front(Print {
            at: at.to_string(),
            price,
            lots,
            tick,
        });
        self.tape.truncate(TAPE);
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
    pub focus: Focus,
    pub orders: Orders,
    /// A ordem sob o cursor no painel Minhas ordens, guardada pelo id.
    pub order_cursor: Option<u64>,
    pub balances: Vec<AssetBalance>,
    pub prompt: Option<Prompt>,
    /// O servidor só manda saldo quando pedido: um pedido por vez, e, se a conta mudou
    /// enquanto ele estava no ar, outro logo depois da resposta.
    balance_pending: bool,
    balance_stale: bool,
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
            focus: Focus::Book,
            orders: Orders::default(),
            order_cursor: None,
            balances: Vec::new(),
            prompt: None,
            balance_pending: false,
            balance_stale: false,
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
            Msg::Server { message, at } => self.on_server(message, &at),
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

    /// Uma quantia de `asset` para gente: em reais se for a quote de algum instrumento, em
    /// unidades inteiras se não.
    pub fn amount(&self, asset: &str, value: i64) -> String {
        if self.markets.iter().any(|market| market.spec.quote == asset) {
            brl(value)
        } else {
            u64::try_from(value).map_or_else(|_| value.to_string(), grouped)
        }
    }

    fn request_balance(&mut self) -> Vec<Effect> {
        if self.balance_pending {
            self.balance_stale = true;
            return Vec::new();
        }
        self.balance_pending = true;
        vec![Effect::Send(ClientMessage::Balance {})]
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

    fn on_server(&mut self, message: ServerMessage, at: &str) -> Vec<Effect> {
        match message {
            ServerMessage::LoggedIn { account } => {
                self.connection = Connection::Online;
                self.say(Tone::Ok, format!("conta {account} conectada"));
                let mut effects = vec![Effect::Send(ClientMessage::Orders {})];
                effects.extend(self.request_balance());
                effects
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
            } => self.on_increment(&instrument, feed, seq, |market| match feed {
                Feed::L1 => market.record_trade(price, lots),
                Feed::L2 => market.print(at, price, lots),
            }),
            ServerMessage::Balances { balances } => {
                self.balances = balances;
                self.balance_pending = false;
                if std::mem::take(&mut self.balance_stale) {
                    self.request_balance()
                } else {
                    Vec::new()
                }
            }
            ServerMessage::Orders { orders } => {
                self.orders.open(orders);
                Vec::new()
            }
            report => self.on_report(report),
        }
    }

    /// Um relatório da conta: muda a lista de ordens, vai para a linha de status e deixa o
    /// saldo velho, que é pedido de novo.
    fn on_report(&mut self, report: ServerMessage) -> Vec<Effect> {
        let Some((tone, text)) = self.describe(&report) else {
            return Vec::new();
        };
        self.track(report);
        self.say(tone, text);
        self.request_balance()
    }

    /// O texto da linha de status; `None` para o que não é relatório da conta.
    fn describe(&self, report: &ServerMessage) -> Option<(Tone, String)> {
        Some(match report {
            ServerMessage::Deposited { asset, amount } => (
                Tone::Ok,
                format!("depósito: {} {asset}", self.amount(asset, *amount)),
            ),
            ServerMessage::DepositRejected { asset, amount } => (
                Tone::Error,
                format!("depósito recusado: {} {asset}", self.amount(asset, *amount)),
            ),
            ServerMessage::Fill {
                instrument,
                role,
                order,
                lots,
                price,
                fee,
            } => {
                let role = match role {
                    Role::Taker => "taker",
                    Role::Maker => "maker",
                };
                (
                    Tone::Ok,
                    format!(
                        "fill #{order} em {instrument}: {lots} lots @ {}, {role}, taxa {}",
                        brl(*price),
                        brl(*fee)
                    ),
                )
            }
            ServerMessage::Rested {
                instrument,
                order,
                price,
                lots,
                ..
            } => (
                Tone::Info,
                format!(
                    "#{order} descansou: {lots} lots @ {} em {instrument}",
                    brl(*price)
                ),
            ),
            ServerMessage::Cancelled {
                instrument,
                order,
                lots,
            } => (
                Tone::Info,
                format!("#{order} cancelada: {lots} lots em {instrument}"),
            ),
            ServerMessage::Expired {
                instrument,
                order,
                lots,
            } => (
                Tone::Info,
                format!("#{order} expirou: {lots} lots em {instrument}"),
            ),
            ServerMessage::Rejected { order, message, .. } => {
                (Tone::Error, format!("#{order} recusada: {message}"))
            }
            ServerMessage::Armed {
                instrument,
                order,
                trigger,
                lots,
                ..
            } => (
                Tone::Info,
                format!(
                    "stop #{order} armado: {lots} lots em {instrument}, gatilho {}",
                    brl(*trigger)
                ),
            ),
            ServerMessage::Triggered { instrument, order } => (
                Tone::Info,
                format!("stop #{order} disparou em {instrument}"),
            ),
            ServerMessage::LoggedIn { .. }
            | ServerMessage::Instruments { .. }
            | ServerMessage::Error { .. }
            | ServerMessage::Snapshot { .. }
            | ServerMessage::Depth { .. }
            | ServerMessage::Top { .. }
            | ServerMessage::Trade { .. }
            | ServerMessage::Balances { .. }
            | ServerMessage::Orders { .. } => return None,
        })
    }

    fn track(&mut self, report: ServerMessage) {
        let orders = &mut self.orders;
        match report {
            ServerMessage::Fill {
                instrument,
                role,
                order,
                lots,
                ..
            } => orders.filled(&instrument, order, lots, role == Role::Taker),
            ServerMessage::Rested {
                instrument,
                order,
                side,
                price,
                lots,
            } => orders.rested(&instrument, order, side, price, lots),
            ServerMessage::Cancelled {
                instrument, order, ..
            } => orders.cancelled(&instrument, order),
            ServerMessage::Expired {
                instrument, order, ..
            } => orders.expired(&instrument, order),
            ServerMessage::Rejected { order, code, .. } => orders.rejected(order, &code),
            ServerMessage::Armed {
                instrument,
                order,
                side,
                trigger,
                lots,
            } => orders.armed(&instrument, order, side, trigger, lots),
            ServerMessage::Triggered { instrument, order } => {
                orders.triggered(&instrument, order);
            }
            // Os demais não mexem em ordem: depósitos só mudam saldo, e o resto nem chega
            // aqui, porque o `describe` não o reconhece como relatório.
            _ => {}
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
        if key == Key::Interrupt {
            return vec![Effect::Quit];
        }
        if let Some(prompt) = self.prompt.take() {
            return if key == Key::Char('y') {
                self.confirm(prompt)
            } else {
                Vec::new()
            };
        }
        if self.help {
            if matches!(key, Key::Char('?') | Key::Esc) {
                self.help = false;
            }
            return Vec::new();
        }
        match (self.tab, key) {
            (_, Key::Char('q')) => match self.orders.open_count() {
                0 => return vec![Effect::Quit],
                open => self.prompt = Some(Prompt::Quit { open }),
            },
            (_, Key::Char('?')) => self.help = true,
            (_, Key::Char(']')) => self.tab = self.next_tab(true),
            (_, Key::Char('[')) => self.tab = self.next_tab(false),
            (Tab::Market, Key::Char('j') | Key::Down) => self.move_cursor(true),
            (Tab::Market, Key::Char('k') | Key::Up) => self.move_cursor(false),
            (Tab::Market, Key::Enter) if self.market_cursor < self.markets.len() => {
                self.tab = Tab::Instrument(self.market_cursor);
            }
            (Tab::Instrument(index), key) => return self.on_instrument_key(index, key),
            (Tab::Market, _) => {}
        }
        Vec::new()
    }

    fn confirm(&mut self, prompt: Prompt) -> Vec<Effect> {
        match prompt {
            Prompt::Quit { .. } => vec![Effect::Quit],
            Prompt::Cancel { instrument, order } => {
                self.say(Tone::Info, format!("cancelando #{order}"));
                vec![Effect::Send(ClientMessage::Cancel { instrument, order })]
            }
        }
    }

    fn on_instrument_key(&mut self, index: usize, key: Key) -> Vec<Effect> {
        let Some(market) = self.markets.get_mut(index) else {
            return Vec::new();
        };
        let tick = market.spec.tick;
        let ticket = &mut market.ticket;
        match (self.focus, key) {
            (_, Key::Tab | Key::BackTab) => {
                self.focus = self.focus.next(key == Key::Tab);
                if self.focus == Focus::Ticket {
                    ticket.enter();
                }
            }
            (_, Key::Char('b')) => ticket.side = Side::Buy,
            (_, Key::Char('s')) => ticket.side = Side::Sell,
            (Focus::Book, Key::Enter) => self.take_price(index),
            (Focus::Orders, Key::Char('x')) => self.ask_cancel(),
            (Focus::Ticket, Key::Enter) => return self.send(index),
            (Focus::Ticket, Key::Char('h') | Key::Left) => ticket.step_field(false),
            (Focus::Ticket, Key::Char('l') | Key::Right) => ticket.step_field(true),
            (Focus::Ticket, Key::Char('j') | Key::Down) => ticket.adjust(false, tick),
            (Focus::Ticket, Key::Char('k') | Key::Up) => ticket.adjust(true, tick),
            (Focus::Ticket, Key::Backspace) => ticket.backspace(),
            (Focus::Ticket, Key::Char(digit)) if digit.is_ascii_digit() => {
                ticket.digit(digit.to_digit(10).unwrap_or(0));
            }
            (_, Key::Char('j') | Key::Down) => self.move_cursor(true),
            (_, Key::Char('k') | Key::Up) => self.move_cursor(false),
            _ => {}
        }
        Vec::new()
    }

    fn take_price(&mut self, index: usize) {
        let Some(market) = self.markets.get_mut(index) else {
            return;
        };
        let Some(selected) = market.selected else {
            self.say(Tone::Info, "escolha um nível do livro com j/k");
            return;
        };
        market.ticket.take_price(selected.price);
        self.focus = Focus::Ticket;
    }

    fn ask_cancel(&mut self) {
        let Some(id) = self.order_cursor else {
            self.say(Tone::Info, "escolha uma ordem com j/k");
            return;
        };
        match self.orders.get(id) {
            Some(order) if order.state.is_open() => {
                self.prompt = Some(Prompt::Cancel {
                    instrument: order.instrument.clone(),
                    order: id,
                });
            }
            Some(_) => self.say(Tone::Info, format!("#{id} já terminou")),
            None => self.say(Tone::Info, "escolha uma ordem com j/k"),
        }
    }

    fn send(&mut self, index: usize) -> Vec<Effect> {
        if self.connection != Connection::Online {
            self.say(Tone::Error, "sem conexão com o servidor");
            return Vec::new();
        }
        let Some(market) = self.markets.get(index) else {
            return Vec::new();
        };
        match market.ticket.order(&market.spec.symbol) {
            Ok((message, pending)) => {
                let text = format!(
                    "enviando: {} em {}",
                    market.ticket.describe(),
                    market.spec.symbol
                );
                self.orders.expect(pending);
                self.say(Tone::Info, text);
                vec![Effect::Send(message)]
            }
            Err(problem) => {
                self.say(Tone::Error, problem);
                Vec::new()
            }
        }
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
            Tab::Instrument(index) => match self.focus {
                Focus::Book => {
                    if let Some(market) = self.markets.get_mut(index) {
                        market.move_selection(down);
                    }
                }
                Focus::Orders => self.move_order_cursor(index, down),
                Focus::Ticket => {}
            },
        }
    }

    /// Sem ordem sob o cursor, ou se ela saiu da lista, começa pela primeira.
    fn move_order_cursor(&mut self, market: usize, down: bool) {
        let Some(market) = self.markets.get(market) else {
            return;
        };
        let ids: Vec<u64> = self
            .orders
            .of(&market.spec.symbol)
            .iter()
            .map(|order| order.id)
            .collect();
        let Some(last) = ids.len().checked_sub(1) else {
            self.order_cursor = None;
            return;
        };
        let index = match ids.iter().position(|&id| Some(id) == self.order_cursor) {
            Some(index) if down => (index + 1).min(last),
            Some(index) => index.saturating_sub(1),
            None => 0,
        };
        self.order_cursor = Some(ids[index]);
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
        app.update(Msg::Server {
            message,
            at: "10:00:00".to_string(),
        })
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

    const BALANCE: Effect = Effect::Send(ClientMessage::Balance {});

    fn balances(available: i64) -> ServerMessage {
        ServerMessage::Balances {
            balances: vec![AssetBalance {
                asset: "BRL".to_string(),
                available,
                reserved: 0,
            }],
        }
    }

    fn rested(order: u64, price: i64, lots: i64) -> ServerMessage {
        ServerMessage::Rested {
            instrument: "TESTE/BRL".to_string(),
            order,
            side: Side::Sell,
            price,
            lots,
        }
    }

    fn fill(order: u64, lots: i64) -> ServerMessage {
        ServerMessage::Fill {
            instrument: "TESTE/BRL".to_string(),
            role: Role::Maker,
            order,
            lots,
            price: 10_000,
            fee: 0,
        }
    }

    #[test]
    fn login_asks_for_open_orders_and_balances() {
        let mut app = App::new("x".to_string(), 1);

        assert_eq!(
            server(&mut app, ServerMessage::LoggedIn { account: 1 }),
            [Effect::Send(ClientMessage::Orders {}), BALANCE]
        );
    }

    #[test]
    fn one_balance_request_at_a_time_and_another_if_the_account_moved_meanwhile() {
        let mut app = ready();
        server(&mut app, balances(1));

        assert_eq!(server(&mut app, rested(1, 10_000, 5)), [BALANCE]);
        assert_eq!(server(&mut app, fill(1, 2)), []);
        assert_eq!(server(&mut app, fill(1, 1)), []);
        assert_eq!(server(&mut app, balances(2)), [BALANCE]);
        assert_eq!(server(&mut app, balances(3)), []);
        assert_eq!(app.balances[0].available, 3);
    }

    #[test]
    fn reports_move_the_orders_and_reach_the_status() {
        let mut app = ready();

        server(&mut app, rested(1, 10_050, 5));
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "#1 descansou: 5 lots @ 100,50 em TESTE/BRL"
        );
        server(&mut app, fill(1, 2));

        let order = app.orders.get(1).unwrap();
        assert_eq!(order.filled, 2);
        assert_eq!(order.remaining, Some(3));
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "fill #1 em TESTE/BRL: 2 lots @ 100,00, maker, taxa 0,00"
        );
    }

    #[test]
    fn open_orders_answer_fills_the_list() {
        let mut app = ready();

        server(
            &mut app,
            ServerMessage::Orders {
                orders: vec![exchange_protocol::OpenOrder {
                    instrument: "TESTE/BRL".to_string(),
                    order: 2,
                    side: Side::Sell,
                    kind: exchange_protocol::OrderType::Stop,
                    price: Some(9_400),
                    trigger: Some(9_500),
                    tif: exchange_protocol::Tif::Gtc,
                    lots: 2,
                    remaining: 2,
                    state: exchange_protocol::OrderState::Armed,
                }],
            },
        );

        assert_eq!(app.orders.of("TESTE/BRL").len(), 1);
        assert_eq!(app.orders.get(2).unwrap().trigger, Some(9_500));
    }

    #[test]
    fn rejection_goes_to_the_status_as_an_error() {
        let mut app = ready();

        server(
            &mut app,
            ServerMessage::Rejected {
                order: 4,
                code: "insufficient_funds".to_string(),
                message: "saldo insuficiente".to_string(),
            },
        );

        let status = app.status.as_ref().unwrap();
        assert_eq!(status.tone, Tone::Error);
        assert_eq!(status.text, "#4 recusada: saldo insuficiente");
    }

    #[test]
    fn l2_trades_fill_the_tape_newest_first_with_their_arrival_time() {
        let mut app = ready();

        server(&mut app, trade(Feed::L2, 5, 10_000));
        app.update(Msg::Server {
            message: trade(Feed::L2, 6, 10_100),
            at: "10:00:05".to_string(),
        });

        let tape = &teste(&app).tape;
        assert_eq!(tape.len(), 2);
        assert_eq!(tape[0].at, "10:00:05");
        assert_eq!(tape[0].tick, Tick::Up);
        assert_eq!(tape[1].price, 10_000);
    }

    #[test]
    fn tape_keeps_only_the_latest() {
        let mut app = ready();

        for seq in 5..5 + u64::try_from(TAPE).unwrap() + 3 {
            server(&mut app, trade(Feed::L2, seq, 10_000));
        }

        assert_eq!(teste(&app).tape.len(), TAPE);
    }

    #[test]
    fn quote_amounts_are_reais_and_base_amounts_are_units() {
        let app = ready();

        assert_eq!(app.amount("BRL", 997_980), "9.979,80");
        assert_eq!(app.amount("TESTE", 100_000), "100.000");
    }

    #[test]
    fn tab_moves_the_focus_only_inside_an_instrument() {
        let mut app = ready();

        app.update(Msg::Key(Key::Tab));
        assert_eq!(app.focus, Focus::Book);
        press(&mut app, "]");
        app.update(Msg::Key(Key::Tab));
        assert_eq!(app.focus, Focus::Orders);
        app.update(Msg::Key(Key::BackTab));
        assert_eq!(app.focus, Focus::Book);
    }

    #[test]
    fn order_cursor_walks_the_list_of_the_open_instrument() {
        let mut app = ready();
        server(&mut app, rested(1, 10_000, 5));
        server(&mut app, rested(2, 10_100, 5));
        press(&mut app, "]");
        app.update(Msg::Key(Key::Tab));

        press(&mut app, "j");
        assert_eq!(app.order_cursor, Some(2));
        press(&mut app, "jj");
        assert_eq!(app.order_cursor, Some(1));
        press(&mut app, "k");
        assert_eq!(app.order_cursor, Some(2));
        assert_eq!(teste(&app).selected, None);
    }

    fn key(app: &mut App, key: Key) -> Vec<Effect> {
        app.update(Msg::Key(key))
    }

    /// Na aba de TESTE/BRL, com o foco na boleta.
    fn at_ticket() -> App {
        let mut app = ready();
        press(&mut app, "]");
        key(&mut app, Key::Tab);
        key(&mut app, Key::Tab);
        assert_eq!(app.focus, Focus::Ticket);
        app
    }

    #[test]
    fn tab_cycles_book_orders_ticket() {
        let mut app = ready();
        press(&mut app, "]");

        key(&mut app, Key::Tab);
        key(&mut app, Key::Tab);
        assert_eq!(app.focus, Focus::Ticket);
        key(&mut app, Key::Tab);
        assert_eq!(app.focus, Focus::Book);
        key(&mut app, Key::BackTab);
        assert_eq!(app.focus, Focus::Ticket);
    }

    #[test]
    fn enter_on_a_book_level_takes_its_price_to_the_ticket() {
        let mut app = ready();
        press(&mut app, "]k");

        key(&mut app, Key::Enter);

        assert_eq!(app.focus, Focus::Ticket);
        assert_eq!(teste(&app).ticket.price, 10_000);
    }

    #[test]
    fn enter_on_the_book_without_a_level_only_explains() {
        let mut app = ready();
        press(&mut app, "]");

        key(&mut app, Key::Enter);

        assert_eq!(app.focus, Focus::Book);
        assert_eq!(teste(&app).ticket.price, 0);
    }

    #[test]
    fn typed_ticket_is_sent_and_awaited() {
        let mut app = at_ticket();
        press(&mut app, "s10050lk");
        press(&mut app, "kkkk");

        let effects = key(&mut app, Key::Enter);

        assert_eq!(
            effects,
            [Effect::Send(ClientMessage::Limit {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Sell,
                price: 10_050,
                lots: 6,
                tif: exchange_protocol::Tif::Gtc,
            })]
        );
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "enviando: venda 6 lots @ 100,50 lim GTC em TESTE/BRL"
        );
        server(&mut app, rested(1, 10_050, 6));
        assert_eq!(app.orders.get(1).unwrap().total, Some(6));
    }

    #[test]
    fn j_and_k_on_the_price_walk_the_instrument_tick() {
        let mut app = at_ticket();

        press(&mut app, "kkk");

        assert_eq!(teste(&app).ticket.price, 30);
    }

    #[test]
    fn incomplete_ticket_is_refused_locally() {
        let mut app = at_ticket();

        assert_eq!(key(&mut app, Key::Enter), []);
        assert_eq!(app.status.as_ref().unwrap().tone, Tone::Error);
    }

    #[test]
    fn nothing_is_sent_while_offline() {
        let mut app = at_ticket();
        press(&mut app, "10050");
        app.update(Msg::Disconnected("caiu".to_string()));

        assert_eq!(key(&mut app, Key::Enter), []);
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "sem conexão com o servidor"
        );
    }

    #[test]
    fn x_asks_and_only_y_cancels() {
        let mut app = ready();
        server(&mut app, rested(1, 10_000, 5));
        press(&mut app, "]");
        key(&mut app, Key::Tab);
        press(&mut app, "j");

        press(&mut app, "x");
        assert_eq!(app.prompt.as_ref().unwrap().question(), "cancelar #1? y/n");
        assert_eq!(key(&mut app, Key::Char('n')), []);
        assert_eq!(app.prompt, None);

        press(&mut app, "x");
        assert_eq!(
            key(&mut app, Key::Char('y')),
            [Effect::Send(ClientMessage::Cancel {
                instrument: "TESTE/BRL".to_string(),
                order: 1,
            })]
        );
    }

    #[test]
    fn finished_order_cannot_be_cancelled() {
        let mut app = ready();
        server(&mut app, rested(1, 10_000, 5));
        server(&mut app, fill(1, 5));
        press(&mut app, "]");
        key(&mut app, Key::Tab);
        press(&mut app, "jx");

        assert_eq!(app.prompt, None);
        assert_eq!(app.status.as_ref().unwrap().text, "#1 já terminou");
    }

    #[test]
    fn q_with_open_orders_asks_first_but_ctrl_c_does_not() {
        let mut app = ready();
        server(&mut app, rested(1, 10_000, 5));

        assert_eq!(key(&mut app, Key::Char('q')), []);
        assert_eq!(
            app.prompt.as_ref().unwrap().question(),
            "há 1 ordem aberta; sair mesmo? y/n"
        );
        assert_eq!(key(&mut app, Key::Char('y')), [Effect::Quit]);

        assert_eq!(key(&mut app, Key::Interrupt), [Effect::Quit]);
    }

    #[test]
    fn b_and_s_choose_the_side_from_any_panel() {
        let mut app = ready();
        press(&mut app, "]s");
        assert_eq!(teste(&app).ticket.side, Side::Sell);
        key(&mut app, Key::Tab);
        press(&mut app, "b");
        assert_eq!(teste(&app).ticket.side, Side::Buy);
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut app = ready();

        assert_eq!(app.update(Msg::Key(Key::Char('q'))), [Effect::Quit]);
        assert_eq!(app.update(Msg::Key(Key::Interrupt)), [Effect::Quit]);
    }
}
