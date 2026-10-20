//! O desenho da tela, função só do estado: a cada quadro tudo é desenhado de novo, e o
//! ratatui reescreve no terminal só o que mudou.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row as TableRow, Table, Tabs};

use exchange_protocol::{Level, Side};

use crate::app::{App, Connection, Market, Row, Tab, Tick, Tone};
use crate::format::{brl, grouped};
use crate::theme;

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
        Some(market) => instrument_tab(frame, body, market),
    }
    status(frame, status_area, app);
    footer(frame, footer_area, app);
    if app.help {
        help(frame, area);
    }
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Span::styled(format!(" {title} "), theme::title()))
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let (dot, state) = match &app.connection {
        Connection::Connecting => (theme::key(), "conectando"),
        Connection::Online => (theme::buy(), "online"),
        Connection::Offline(_) => (theme::sell(), "offline"),
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
        .highlight_style(theme::key().add_modifier(ratatui::style::Modifier::BOLD))
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
            Cell::from(Span::styled(quote(bid), theme::buy())),
            Cell::from(Span::styled(quote(ask), theme::sell())),
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

fn instrument_tab(frame: &mut Frame, area: Rect, market: &Market) {
    let [main, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(4)]).areas(area);
    let [book_area, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main);
    let [trades, orders] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);
    let [ticket, balances] =
        Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)]).areas(bottom);

    book(frame, book_area, market);
    for (title, area) in [
        ("Negócios", trades),
        ("Minhas ordens", orders),
        ("Boleta", ticket),
        ("Saldos", balances),
    ] {
        frame.render_widget(panel(title, false), area);
    }
}

fn book(frame: &mut Frame, area: Rect, market: &Market) {
    let block = panel(&format!("Livro {}", market.spec.symbol), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if market.depth.is_none() {
        let waiting = Paragraph::new(Span::styled("aguardando snapshot…", theme::dim()));
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
        usize::from(inner.width).saturating_sub(MARKER_WIDTH + PRICE_WIDTH + LOTS_WIDTH + 2);

    let mut lines = vec![Line::styled(
        format!(
            "{:MARKER_WIDTH$}{:>PRICE_WIDTH$} {:>LOTS_WIDTH$}",
            "", "preço", "lots"
        ),
        theme::dim(),
    )];
    let line = |row: &Row| level_line(row, market.is_selected(row), widest, bar_width);
    lines.extend(visible[..shown_asks].iter().map(line));
    lines.push(spread_line(market, usize::from(inner.width)));
    lines.extend(visible[shown_asks..].iter().map(line));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn level_line(row: &Row, selected: bool, widest: i64, bar_width: usize) -> Line<'static> {
    let side = match row.side {
        Side::Buy => theme::buy(),
        Side::Sell => theme::sell(),
    };
    let marker = if selected { "▸ " } else { "  " };
    let line = Line::from(vec![
        Span::styled(marker, theme::key()),
        Span::styled(format!("{:>PRICE_WIDTH$}", brl(row.level.price)), side),
        Span::raw(format!(" {:>LOTS_WIDTH$} ", lots(row.level.lots))),
        Span::styled(bar(row.level.lots, widest, bar_width), side),
    ]);
    if selected {
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
    if let Some(status) = &app.status {
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
        Tab::Instrument(_) => &[("j/k", "nível"), ("[ ]", "aba"), ("q", "sair")],
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

const SHORTCUTS: [(&str, &str); 7] = [
    ("j k ↓ ↑", "mover dentro do painel"),
    ("[ ]", "aba anterior / próxima"),
    ("↵", "na aba Mercado, abrir o instrumento"),
    ("?", "abrir / fechar esta ajuda"),
    ("esc", "fechar esta ajuda"),
    ("q", "sair"),
    ("ctrl-c", "sair"),
];

fn help(frame: &mut Frame, area: Rect) {
    let height = u16::try_from(SHORTCUTS.len()).unwrap_or(u16::MAX) + 2;
    let popup = centered(area, 52, height);
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
            app.update(Msg::Server(message));
        }
        app
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
