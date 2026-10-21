//! O desenho da tela, função só do estado: a cada quadro tudo é desenhado de novo, e o
//! ratatui reescreve no terminal só o que mudou.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row as TableRow, Table, Tabs};

use exchange_protocol::{Level, OrderType, Side, Tif};

use crate::app::{App, Connection, Focus, Market, Row, Tab, Tick, Tone};
use crate::format::{brl, grouped};
use crate::orders::{Order, State};
use crate::theme;
use crate::ticket::{Field, tif_label};

const PRICE_WIDTH: usize = 12;
const LOTS_WIDTH: usize = 8;
const MARKER_WIDTH: usize = 2;
const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(theme::base()), area);
    let [header_area, body, status_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    header(frame, header_area, app);
    match app.current() {
        None => market_tab(frame, body, app),
        Some(market) => instrument_tab(frame, body, app, market),
    }
    status(frame, status_area, app);
    footer(frame, footer_area, app);
    if app.help {
        help(frame, area);
    }
}

/// A cor de um número de mercado, ou cinza se a conexão caiu e ele é velho.
fn live(app: &App, style: Style) -> Style {
    if app.is_online() { style } else { theme::dim() }
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Span::styled(format!(" {title} "), theme::title()))
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let (dot, state) = match &app.connection {
        Connection::Connecting => (theme::key(), "conectando".to_string()),
        Connection::Online => (theme::buy(), "online".to_string()),
        Connection::Reconnecting { attempt, .. } => {
            (theme::sell(), format!("reconectando ({attempt})"))
        }
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::dim())
        .title(Line::from(vec![
            Span::styled(" exchange ", theme::title()),
            Span::styled(format!("─ conta {} ", app.account), theme::dim()),
        ]))
        .title_top(
            Line::from(vec![
                Span::styled(format!(" {} ", app.server), theme::dim()),
                Span::styled("● ", dot),
                Span::styled(format!("{state} "), theme::dim()),
            ])
            .right_aligned(),
        );
    let titles: Vec<Line> = std::iter::once("Mercado".to_string())
        .chain(app.markets.iter().map(|market| market.spec.symbol.clone()))
        .map(Line::from)
        .collect();
    let selected = match app.tab {
        Tab::Market => 0,
        Tab::Instrument(index) => index + 1,
    };
    let tabs = Tabs::new(titles)
        .select(Some(selected))
        .style(theme::dim())
        .highlight_style(theme::key().add_modifier(Modifier::BOLD))
        .divider("   ")
        .block(block);
    frame.render_widget(tabs, area);
}

fn market_tab(frame: &mut Frame, area: Rect, app: &App) {
    let block = panel("Mercado", true);
    if app.markets.is_empty() {
        let waiting =
            Paragraph::new(Span::styled("carregando instrumentos…", theme::dim())).block(block);
        frame.render_widget(waiting, area);
        return;
    }
    let header =
        TableRow::new(["  instrumento", "compra", "venda", "spread", "último"]).style(theme::dim());
    let rows = app.markets.iter().enumerate().map(|(index, market)| {
        let chosen = index == app.market_cursor;
        let top = market.top.as_ref();
        let bid = top.and_then(crate::book::Book::best_bid);
        let ask = top.and_then(crate::book::Book::best_ask);
        let marker = if chosen { "▸ " } else { "  " };
        let row = TableRow::new([
            Cell::from(Line::from(vec![
                Span::styled(marker, theme::key()),
                Span::raw(market.spec.symbol.clone()),
            ])),
            Cell::from(Span::styled(quote(bid), live(app, theme::buy()))),
            Cell::from(Span::styled(quote(ask), live(app, theme::sell()))),
            Cell::from(spread(bid, ask)),
            Cell::from(last(market)),
        ]);
        if chosen {
            row.style(theme::selected())
        } else {
            row
        }
    });
    let widths = [
        Constraint::Length(16),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(10),
        Constraint::Min(12),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn quote(level: Option<Level>) -> String {
    level.map_or_else(
        || "—".to_string(),
        |level| format!("{} ×{}", brl(level.price), lots(level.lots)),
    )
}

fn spread(bid: Option<Level>, ask: Option<Level>) -> String {
    match (bid, ask) {
        (Some(bid), Some(ask)) => brl(ask.price - bid.price),
        _ => "—".to_string(),
    }
}

fn last(market: &Market) -> Line<'static> {
    let Some(trade) = market.last else {
        return Line::styled("—", theme::dim());
    };
    let arrow = match trade.tick {
        Tick::Up => Span::styled(" ▲", theme::buy()),
        Tick::Down => Span::styled(" ▼", theme::sell()),
        Tick::Flat => Span::raw(""),
    };
    Line::from(vec![Span::styled(brl(trade.price), theme::last()), arrow])
}

fn lots(lots: i64) -> String {
    u64::try_from(lots).map_or_else(|_| lots.to_string(), grouped)
}

fn instrument_tab(frame: &mut Frame, area: Rect, app: &App, market: &Market) {
    let [main, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(area);
    let [book_area, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main);
    let [trades, orders] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);
    let [ticket, balances] =
        Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)]).areas(bottom);

    book(
        frame,
        book_area,
        market,
        app.focus == Focus::Book,
        app.is_online(),
    );
    tape(frame, trades, market);
    order_list(frame, orders, app, market);
    ticket_panel(frame, ticket, market, app.focus == Focus::Ticket);
    balance_list(frame, balances, app);
}

fn tape(frame: &mut Frame, area: Rect, market: &Market) {
    let block = panel("Negócios", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if market.tape.is_empty() {
        let waiting = Paragraph::new(Span::styled(" nenhum negócio ainda", theme::dim()));
        frame.render_widget(waiting, inner);
        return;
    }
    let lines: Vec<Line> = market
        .tape
        .iter()
        .take(usize::from(inner.height))
        .map(|print| {
            let (style, arrow) = match print.tick {
                Tick::Up => (theme::buy(), "▲"),
                Tick::Down => (theme::sell(), "▼"),
                Tick::Flat => (theme::base(), " "),
            };
            Line::from(vec![
                Span::styled(format!(" {}  ", print.at), theme::dim()),
                Span::styled(format!("{:>PRICE_WIDTH$}", brl(print.price)), style),
                Span::raw(format!(" {:>LOTS_WIDTH$}  ", lots(print.lots))),
                Span::styled(arrow, style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn order_list(frame: &mut Frame, area: Rect, app: &App, market: &Market) {
    let block = panel("Minhas ordens", app.focus == Focus::Orders);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let orders = app.orders.of(&market.spec.symbol);
    if orders.is_empty() {
        let none = Paragraph::new(Span::styled(
            " nenhuma ordem neste instrumento",
            theme::dim(),
        ));
        frame.render_widget(none, inner);
        return;
    }
    let shown = usize::from(inner.height);
    let selected = orders
        .iter()
        .position(|order| Some(order.id) == app.order_cursor);
    // A janela anda junto com o cursor quando a lista não cabe.
    let start = selected.map_or(0, |index| (index + 1).saturating_sub(shown));
    let lines: Vec<Line> = orders
        .iter()
        .skip(start)
        .take(shown)
        .map(|order| order_line(order, Some(order.id) == app.order_cursor))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn order_line(order: &Order, selected: bool) -> Line<'static> {
    let open = order.state.is_open();
    let tint = |style| if open { style } else { theme::dim() };
    let (side, side_style) = match order.side {
        Some(Side::Buy) => ("compra", theme::buy()),
        Some(Side::Sell) => ("venda", theme::sell()),
        None => ("—", theme::dim()),
    };
    let kind = match order.kind {
        Some(OrderType::Limit) => "lim",
        Some(OrderType::Market) => "mkt",
        Some(OrderType::Stop) => "stop",
        None => "—",
    };
    // Armado, o que importa é o gatilho: na compra dispara no gatilho ou acima, na venda no
    // gatilho ou abaixo.
    let price = match (&order.state, order.trigger, order.side) {
        (State::Armed, Some(trigger), Some(Side::Buy)) => format!("≥{}", brl(trigger)),
        (State::Armed, Some(trigger), Some(Side::Sell)) => format!("≤{}", brl(trigger)),
        _ => order.price.map_or_else(|| "—".to_string(), brl),
    };
    let total = order.total.map_or_else(|| "?".to_string(), lots);
    let tif = match order.tif {
        Some(Tif::Gtc) => "GTC",
        Some(Tif::Ioc) => "IOC",
        Some(Tif::Fok) => "FOK",
        None => "—",
    };
    let state = match order.state {
        State::Accepted => "receb.",
        State::Resting => "aberta",
        State::Armed => "armada",
        State::Filled => "exec.",
        State::Cancelled => "cancel.",
        State::Expired => "expir.",
        State::Rejected(_) => "recus.",
        State::Closed => "encerr.",
    };
    let marker = if selected { "▸ " } else { "  " };
    let line = Line::from(vec![
        Span::styled(marker, theme::key()),
        Span::styled(format!("#{:<4} ", order.id), tint(theme::base())),
        Span::styled(format!("{side:<6} "), tint(side_style)),
        Span::styled(format!("{kind:<4} "), tint(theme::base())),
        Span::styled(format!("{price:>10} "), tint(theme::base())),
        Span::styled(
            format!("{:>7} ", format!("{}/{total}", lots(order.filled))),
            tint(theme::base()),
        ),
        Span::styled(format!("{tif} "), tint(theme::dim())),
        Span::styled(state, tint(theme::key())),
    ]);
    if selected {
        line.style(theme::selected())
    } else {
        line
    }
}

fn ticket_panel(frame: &mut Frame, area: Rect, market: &Market, focused: bool) {
    let block = panel("Boleta", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let ticket = &market.ticket;
    let field = |name: &str, value: String, which: Field| {
        let chosen = focused && ticket.field == which;
        let style = if chosen {
            theme::key().add_modifier(Modifier::BOLD)
        } else {
            theme::base()
        };
        let brackets = if chosen { theme::key() } else { theme::dim() };
        let mut spans = Vec::new();
        if !name.is_empty() {
            spans.push(Span::styled(format!("{name} "), theme::dim()));
        }
        spans.extend([
            Span::styled("[", brackets),
            Span::styled(value, style),
            Span::styled("]", brackets),
            Span::raw("  "),
        ]);
        spans
    };
    let side = match ticket.side {
        Side::Buy => Span::styled("COMPRA", theme::buy().add_modifier(Modifier::BOLD)),
        Side::Sell => Span::styled("VENDA", theme::sell().add_modifier(Modifier::BOLD)),
    };
    let side_chosen = focused && ticket.field == Field::Side;
    let brackets = if side_chosen {
        theme::key()
    } else {
        theme::dim()
    };
    let mut first = vec![
        Span::raw(" "),
        Span::styled("[", brackets),
        side,
        Span::styled("]", brackets),
        Span::raw("  "),
    ];
    let kind = match ticket.kind {
        OrderType::Limit => "lim",
        OrderType::Market => "mkt",
        OrderType::Stop => "stop",
    };
    first.extend(field("tipo", kind.to_string(), Field::Kind));
    first.extend(field("tif", tif_label(ticket.tif).to_string(), Field::Tif));

    let mut second = vec![Span::raw(" ")];
    if ticket.fields().contains(&Field::Trigger) {
        second.extend(field("gatilho", money(ticket.trigger), Field::Trigger));
    }
    if ticket.fields().contains(&Field::Price) {
        let name = if ticket.kind == OrderType::Stop {
            "limite"
        } else {
            "preço"
        };
        second.extend(field(name, money(ticket.price), Field::Price));
    }
    second.extend(field(
        "lots",
        format!("{:>5}", lots(ticket.lots)),
        Field::Lots,
    ));

    let hint = if focused {
        Line::from(vec![
            Span::styled(" ↵ ", theme::key()),
            Span::styled(format!("enviar {}", ticket.describe()), theme::dim()),
        ])
    } else {
        Line::styled(
            " tab até aqui para editar · b/s escolhe o lado",
            theme::dim(),
        )
    };
    let lines = vec![Line::from(first), Line::from(second), hint];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Zero é campo vazio.
fn money(value: i64) -> String {
    if value == 0 {
        format!("{:>9}", "—")
    } else {
        format!("{:>9}", brl(value))
    }
}

fn balance_list(frame: &mut Frame, area: Rect, app: &App) {
    let block = panel("Saldos", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.balances.is_empty() {
        frame.render_widget(Paragraph::new(Span::styled(" …", theme::dim())), inner);
        return;
    }
    let lines: Vec<Line> = app
        .balances
        .iter()
        .map(|balance| {
            Line::from(vec![
                Span::styled(format!(" {:<6}", balance.asset), theme::title()),
                Span::raw(format!(
                    "{:>14}",
                    app.amount(&balance.asset, balance.available)
                )),
                Span::styled("  res ", theme::dim()),
                Span::styled(app.amount(&balance.asset, balance.reserved), theme::dim()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn book(frame: &mut Frame, area: Rect, market: &Market, focused: bool, online: bool) {
    let block = panel(&format!("Livro {}", market.spec.symbol), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if market.depth.is_none() {
        let waiting = Paragraph::new(Span::styled(" aguardando snapshot…", theme::dim()));
        frame.render_widget(waiting, inner);
        return;
    }

    let ladder = market.ladder();
    let asks = ladder.iter().filter(|row| row.side == Side::Sell).count();
    let room = usize::from(inner.height).saturating_sub(2);
    let bids = ladder.len() - asks;
    let shown_asks = asks.min(room.saturating_sub(bids.min(room / 2)));
    let shown_bids = bids.min(room - shown_asks);
    let visible = &ladder[asks - shown_asks..asks + shown_bids];
    let widest = visible.iter().map(|row| row.level.lots).max().unwrap_or(0);
    let bar_width =
        usize::from(inner.width).saturating_sub(MARKER_WIDTH + PRICE_WIDTH + LOTS_WIDTH + 3);

    let mut lines = vec![Line::styled(
        format!(
            "{:MARKER_WIDTH$}{:>PRICE_WIDTH$} {:>LOTS_WIDTH$}",
            "", "preço", "lots"
        ),
        theme::dim(),
    )];
    let line = |row: &Row| {
        let look = Look {
            selected: market.is_selected(row),
            flashing: market.is_flashing(row),
            live: online,
        };
        level_line(row, look, widest, bar_width)
    };
    lines.extend(visible[..shown_asks].iter().map(line));
    lines.push(spread_line(market, usize::from(inner.width)));
    lines.extend(visible[shown_asks..].iter().map(line));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Como uma linha do livro aparece. Sem conexão (`live` falso) tudo fica cinza: os
/// números são do instante da queda, não do mercado de agora.
#[derive(Clone, Copy)]
struct Look {
    selected: bool,
    flashing: bool,
    live: bool,
}

fn level_line(row: &Row, look: Look, widest: i64, bar_width: usize) -> Line<'static> {
    let side = match (look.live, row.side) {
        (false, _) => theme::dim(),
        (true, Side::Buy) => theme::buy(),
        (true, Side::Sell) => theme::sell(),
    };
    let quantity = match (look.live, look.flashing) {
        (false, _) => theme::dim(),
        (true, true) => theme::last().add_modifier(Modifier::BOLD),
        (true, false) => Style::new(),
    };
    let marker = if look.selected { "▸ " } else { "  " };
    let line = Line::from(vec![
        Span::styled(marker, theme::key()),
        Span::styled(format!("{:>PRICE_WIDTH$}", brl(row.level.price)), side),
        Span::styled(format!(" {:>LOTS_WIDTH$} ", lots(row.level.lots)), quantity),
        Span::styled(bar(row.level.lots, widest, bar_width), side),
    ]);
    if look.selected {
        line.style(theme::selected())
    } else {
        line
    }
}

fn spread_line(market: &Market, width: usize) -> Line<'static> {
    let book = market.depth.as_ref();
    let spread = spread(
        book.and_then(crate::book::Book::best_bid),
        book.and_then(crate::book::Book::best_ask),
    );
    let mut spans = vec![Span::styled(format!(" spread {spread} "), theme::dim())];
    if let Some(trade) = market.last {
        spans.push(Span::styled("─ último ", theme::dim()));
        spans.push(Span::styled(
            format!("{} ", brl(trade.price)),
            theme::last(),
        ));
    }
    let text: usize = spans.iter().map(Span::width).sum();
    let rule = "─".repeat(width.saturating_sub(text) / 2);
    let mut line = vec![Span::styled(rule.clone(), theme::dim())];
    line.extend(spans);
    line.push(Span::styled(rule, theme::dim()));
    Line::from(line)
}

/// Barra de `lots` proporcional a `widest` em `width` colunas, com oitavos de bloco na ponta.
fn bar(lots: i64, widest: i64, width: usize) -> String {
    let (Ok(lots), Ok(widest), Ok(width)) = (
        u128::try_from(lots),
        u128::try_from(widest),
        u128::try_from(width),
    ) else {
        return String::new();
    };
    if widest == 0 {
        return String::new();
    }
    let eighths = lots * width * 8 / widest;
    let full = usize::try_from(eighths / 8).unwrap_or(0);
    let partial = usize::try_from(eighths % 8).unwrap_or(0);
    format!("{}{}", "█".repeat(full), PARTIAL[partial])
}

fn status(frame: &mut Frame, area: Rect, app: &App) {
    let [left, right] = Layout::horizontal([Constraint::Min(0), Constraint::Length(9)]).areas(area);
    if let Some(prompt) = &app.prompt {
        let line = Line::from(vec![
            Span::styled(" ? ", theme::key()),
            Span::styled(prompt.question(), theme::key().add_modifier(Modifier::BOLD)),
        ]);
        frame.render_widget(Paragraph::new(line), left);
    } else if let Some(status) = &app.status {
        let (mark, style) = match status.tone {
            Tone::Info => ("·", theme::base()),
            Tone::Ok => ("✓", theme::buy()),
            Tone::Error => ("✗", theme::sell()),
        };
        let line = Line::from(vec![
            Span::styled(format!(" {mark} "), style),
            Span::styled(status.text.clone(), style),
        ]);
        frame.render_widget(Paragraph::new(line), left);
    }
    let help = Line::from(vec![
        Span::styled("? ", theme::key()),
        Span::styled("ajuda ", theme::dim()),
    ])
    .alignment(Alignment::Right);
    frame.render_widget(Paragraph::new(help), right);
}

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let hints: &[(&str, &str)] = match app.tab {
        Tab::Market => &[
            ("j/k", "mover"),
            ("↵", "abrir"),
            ("[ ]", "aba"),
            ("q", "sair"),
        ],
        Tab::Instrument(_) => match app.focus {
            Focus::Book => &[
                ("j/k", "nível"),
                ("↵", "usar preço"),
                ("tab", "ordens"),
                ("[ ]", "aba"),
                ("q", "sair"),
            ],
            Focus::Orders => &[
                ("j/k", "ordem"),
                ("x", "cancelar"),
                ("tab", "boleta"),
                ("q", "sair"),
            ],
            Focus::Ticket => &[
                ("h/l", "campo"),
                ("j/k", "ajustar"),
                ("0-9", "digitar"),
                ("b/s", "lado"),
                ("↵", "enviar"),
                ("tab", "livro"),
            ],
        },
    };
    let mut spans = vec![Span::raw(" ")];
    for (index, (key, action)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(*key, theme::key()));
        spans.push(Span::styled(format!(" {action}"), theme::dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

const SHORTCUTS: [(&str, &str); 14] = [
    ("j k ↓ ↑", "mover; na boleta, ajustar o campo"),
    ("h l ← →", "na boleta, campo anterior / próximo"),
    ("tab", "foco: livro → ordens → boleta"),
    ("[ ]", "aba anterior / próxima"),
    ("↵", "Mercado: abrir · livro: usar o preço"),
    ("↵", "na boleta, enviar a ordem"),
    ("0-9 ⌫", "na boleta, digitar (centavos)"),
    ("b s", "lado da boleta: compra / venda"),
    ("x", "nas ordens, cancelar (pede y)"),
    ("y", "confirmar a pergunta da status"),
    ("?", "abrir / fechar esta ajuda"),
    ("esc", "fechar esta ajuda"),
    ("q", "sair (pede y com ordem aberta)"),
    ("ctrl-c", "sair na hora"),
];

fn help(frame: &mut Frame, area: Rect) {
    let height = u16::try_from(SHORTCUTS.len()).unwrap_or(u16::MAX) + 2;
    let popup = centered(area, 56, height);
    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = SHORTCUTS
        .iter()
        .map(|(key, action)| {
            Line::from(vec![
                Span::styled(format!(" {key:>8}  "), theme::key()),
                Span::raw(*action),
            ])
        })
        .collect();
    let block = panel("Atalhos", true).style(theme::base());
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use exchange_protocol::{Feed, InstrumentSpec, ServerMessage};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{Key, Msg};

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn trading() -> App {
        let mut app = App::new("127.0.0.1:7000".to_string(), 1);
        let messages = [
            ServerMessage::LoggedIn { account: 1 },
            ServerMessage::Instruments {
                instruments: vec![InstrumentSpec {
                    symbol: "TESTE/BRL".to_string(),
                    base: "TESTE".to_string(),
                    quote: "BRL".to_string(),
                    tick: 10,
                    lot_size: 100,
                    max_lots: 1_000,
                }],
            },
            ServerMessage::Snapshot {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2,
                seq: 0,
                bids: vec![Level {
                    price: 9_990,
                    lots: 9,
                }],
                asks: vec![Level {
                    price: 1_005_000,
                    lots: 1_200,
                }],
            },
        ];
        for message in messages {
            app.update(Msg::Server {
                message,
                at: "10:41:07".to_string(),
            });
        }
        app
    }

    /// A cor do texto na primeira célula onde `text` aparece na tela.
    fn color_of(app: &App, text: &str) -> ratatui::style::Color {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let cells: Vec<&str> = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            let line = cells.concat();
            if let Some(byte) = line.find(text) {
                let x = line[..byte].chars().count();
                return buffer[(u16::try_from(x).unwrap(), y)].fg;
            }
        }
        panic!("{text} não está na tela");
    }

    fn depth(seq: u64, lots: i64) -> Msg {
        Msg::Server {
            message: ServerMessage::Depth {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2,
                seq,
                side: Side::Buy,
                price: 9_990,
                lots,
            },
            at: "10:41:08".to_string(),
        }
    }

    #[test]
    fn changed_level_shows_its_lots_in_orange_until_the_flash_ends() {
        let mut app = trading();
        app.update(Msg::Key(Key::Char(']')));
        app.update(depth(1, 7));

        assert_eq!(color_of(&app, "     7 "), theme::ORANGE);
        for _ in 0..crate::app::FLASH_TICKS {
            app.update(Msg::Tick);
        }
        assert_ne!(color_of(&app, "     7 "), theme::ORANGE);
    }

    #[test]
    fn lost_connection_greys_the_book_and_says_it_is_reconnecting() {
        let mut app = trading();
        app.update(Msg::Key(Key::Char(']')));
        assert_eq!(color_of(&app, "99,90"), theme::GREEN);

        app.update(Msg::Disconnected("o servidor fechou a conexão".to_string()));
        app.update(Msg::Disconnected("conexão recusada".to_string()));

        let screen = screen(&app);
        assert!(screen.contains("reconectando (2)"), "{screen}");
        assert!(screen.contains("tentando de novo a cada 2s"), "{screen}");
        assert_eq!(color_of(&app, "99,90"), theme::GRAY);
    }

    #[test]
    fn market_tab_lists_the_instruments_before_any_quote() {
        let screen = screen(&trading());

        assert!(screen.contains("TESTE/BRL"), "{screen}");
        assert!(screen.contains("online"), "{screen}");
        assert!(screen.contains("—"), "{screen}");
    }

    #[test]
    fn instrument_tab_draws_the_book_in_reais_around_the_spread() {
        let mut app = trading();
        app.update(Msg::Key(Key::Char(']')));

        let screen = screen(&app);

        assert!(screen.contains("Livro TESTE/BRL"), "{screen}");
        assert!(screen.contains("10.050,00"), "{screen}");
        assert!(screen.contains("1.200"), "{screen}");
        assert!(screen.contains("99,90"), "{screen}");
        assert!(screen.contains("spread 9.950,10"), "{screen}");
        let ask = screen.find("10.050,00").unwrap();
        let bid = screen.find("99,90").unwrap();
        assert!(ask < bid, "vendas em cima: {screen}");
    }

    #[test]
    fn instrument_tab_shows_tape_orders_and_balances() {
        let mut app = trading();
        let messages = [
            ServerMessage::Trade {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2,
                seq: 1,
                price: 10_050,
                lots: 3,
            },
            ServerMessage::Rested {
                instrument: "TESTE/BRL".to_string(),
                order: 12,
                side: Side::Buy,
                price: 9_990,
                lots: 5,
            },
            ServerMessage::Balances {
                balances: vec![exchange_protocol::AssetBalance {
                    asset: "BRL".to_string(),
                    available: 997_980,
                    reserved: 49_950,
                }],
            },
        ];
        for message in messages {
            app.update(Msg::Server {
                message,
                at: "10:41:07".to_string(),
            });
        }
        app.update(Msg::Key(Key::Char(']')));

        let screen = screen(&app);

        assert!(screen.contains("10:41:07"), "{screen}");
        assert!(screen.contains("#12"), "{screen}");
        assert!(screen.contains("compra"), "{screen}");
        assert!(screen.contains("0/5"), "{screen}");
        assert!(screen.contains("aberta"), "{screen}");
        assert!(screen.contains("9.979,80"), "{screen}");
        assert!(screen.contains("res 499,50"), "{screen}");
    }

    #[test]
    fn help_lists_the_shortcuts() {
        let mut app = trading();
        app.update(Msg::Key(Key::Char('?')));

        assert!(screen(&app).contains("Atalhos"));
    }

    #[test]
    fn bar_is_proportional_with_eighths_at_the_tip() {
        assert_eq!(bar(10, 10, 4), "████");
        assert_eq!(bar(5, 10, 4), "██");
        assert_eq!(bar(3, 10, 4), "█▏");
        assert_eq!(bar(0, 10, 4), "");
        assert_eq!(bar(5, 0, 4), "");
        assert_eq!(bar(5, 10, 0), "");
    }
}
