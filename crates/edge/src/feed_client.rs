//! O assinante do feed pela rede (seção 4.3; critério da F5): recebe o snapshot e os
//! incrementos, detecta buraco pela sequência e se recupera pedindo um snapshot novo.

use std::fmt::Write as _;
use std::io;
use std::ops::ControlFlow;

use exchange_core::domain::InstrumentId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::market::{instrument_id, symbol};
use crate::market_data::{Feed, Gap, LocalBook};
use crate::protocol::{self, FeedMessage, ServerMessage};

/// O que fazer depois de uma mensagem do servidor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// O livro mudou (ou acabou de ser montado): vale redesenhar.
    Redraw,
    /// Nada para este assinante.
    Ignore,
    /// Faltou um incremento; o livro local foi descartado e é preciso assinar de novo.
    Resubscribe(Gap),
    ServerError(String),
}

/// A lógica do assinante, sem rede: as mensagens entram, as decisões saem.
pub struct Tracker {
    instrument: InstrumentId,
    feed: Feed,
    book: Option<LocalBook>,
}

impl Tracker {
    pub fn new(instrument: InstrumentId, feed: Feed) -> Self {
        Self {
            instrument,
            feed,
            book: None,
        }
    }

    /// O livro local; `None` enquanto espera um snapshot.
    pub fn book(&self) -> Option<&LocalBook> {
        self.book.as_ref()
    }

    /// Decide o que fazer com uma mensagem do servidor. No buraco, o livro é descartado: os
    /// incrementos seguintes não podem ser aplicados sobre um estado que sabemos incompleto,
    /// e são ignorados até o snapshot novo chegar.
    pub fn handle(&mut self, message: ServerMessage) -> Action {
        if let ServerMessage::Error { message } = message {
            return Action::ServerError(message);
        }
        match message.into_feed() {
            Some(FeedMessage::Snapshot(snapshot))
                if snapshot.instrument == self.instrument && snapshot.feed == self.feed =>
            {
                self.book = Some(LocalBook::from_snapshot(&snapshot));
                Action::Redraw
            }
            Some(FeedMessage::Update(update))
                if update.instrument == self.instrument && update.feed == self.feed =>
            {
                let Some(book) = &mut self.book else {
                    return Action::Ignore;
                };
                match book.apply(&update) {
                    Ok(()) => Action::Redraw,
                    Err(gap) => {
                        self.book = None;
                        Action::Resubscribe(gap)
                    }
                }
            }
            _ => Action::Ignore,
        }
    }
}

fn subscribe_line(instrument: &str, feed: Feed) -> String {
    let message = serde_json::json!({
        "type": "subscribe",
        "instrument": instrument,
        "feed": protocol::Feed::from(feed),
    });
    format!("{message}\n")
}

/// Assina `instrument` no fluxo `feed` e segue o livro até o servidor fechar a conexão ou
/// `view` pedir para parar. A cada mensagem, `view` recebe o estado e a ação tomada; num
/// buraco, a assinatura é refeita sozinha.
///
/// # Errors
/// Se o instrumento não existir no mercado, ou se a conexão falhar.
pub async fn follow(
    stream: TcpStream,
    instrument: &str,
    feed: Feed,
    mut view: impl FnMut(&Tracker, &Action) -> ControlFlow<()>,
) -> io::Result<()> {
    let id = instrument_id(instrument).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("instrumento desconhecido \"{instrument}\""),
        )
    })?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let subscribe = subscribe_line(instrument, feed);
    writer.write_all(subscribe.as_bytes()).await?;

    let mut tracker = Tracker::new(id, feed);
    while let Some(line) = lines.next_line().await? {
        // Mensagem que este cliente não entende (de um servidor mais novo) é ignorada.
        let Ok(message) = serde_json::from_str::<ServerMessage>(&line) else {
            continue;
        };
        let action = tracker.handle(message);
        if let Action::Resubscribe(_) = action {
            writer.write_all(subscribe.as_bytes()).await?;
        }
        if view(&tracker, &action).is_break() {
            break;
        }
    }
    Ok(())
}

/// O livro em texto: vendas em cima (a pior no alto), o spread, compras embaixo, e o último
/// negócio. No máximo `depth` níveis por lado.
pub fn render(book: &LocalBook, depth: usize) -> String {
    let snapshot = book.snapshot();
    let feed = match snapshot.feed {
        Feed::L1 => "L1",
        Feed::L2 => "L2",
    };
    let mut out = format!(
        "{}  {feed}  seq {}\n        preço     lots\n",
        symbol(snapshot.instrument),
        snapshot.seq
    );
    for ask in snapshot.asks.iter().take(depth).rev() {
        let _ = writeln!(out, "venda  {:>6} {:>8}", ask.price.get(), ask.lots.get());
    }
    match (snapshot.bids.first(), snapshot.asks.first()) {
        (Some(bid), Some(ask)) => {
            let spread = ask.price.get() - bid.price.get();
            let _ = writeln!(out, "------ spread {spread} ------");
        }
        _ => out.push_str("------------------------\n"),
    }
    for bid in snapshot.bids.iter().take(depth) {
        let _ = writeln!(out, "compra {:>6} {:>8}", bid.price.get(), bid.lots.get());
    }
    match book.last_trade() {
        Some(trade) => {
            let _ = writeln!(
                out,
                "último negócio: {} lots @ {}",
                trade.lots.get(),
                trade.price.get()
            );
        }
        None => out.push_str("último negócio: —\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use exchange_core::domain::{Lots, Price, Side};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;
    use crate::core_thread::spawn_core;
    use crate::market::{INSTRUMENTS, new_market_data};
    use crate::market_data::{Body, Quote, Snapshot, Update};
    use crate::sequencer::Sequencer;
    use crate::server::{Limits, serve};

    const TESTE_BRL: InstrumentId = INSTRUMENTS[0].1;
    const PATIENCE: Duration = Duration::from_secs(5);

    fn quote(price: i64, lots: i64) -> Quote {
        Quote {
            price: Price::new(price).unwrap(),
            lots: Lots::new(lots).unwrap(),
        }
    }

    fn snapshot(seq: u64, asks: Vec<Quote>) -> ServerMessage {
        ServerMessage::from(&Snapshot {
            instrument: TESTE_BRL,
            feed: Feed::L2,
            seq,
            bids: vec![],
            asks,
        })
    }

    fn ask_level(seq: u64, price: i64, lots: i64) -> ServerMessage {
        ServerMessage::from(&Update {
            instrument: TESTE_BRL,
            feed: Feed::L2,
            seq,
            body: Body::Level {
                side: Side::Ask,
                price: Price::new(price).unwrap(),
                lots: Lots::new(lots).unwrap(),
            },
        })
    }

    fn asks(tracker: &Tracker) -> Vec<Quote> {
        tracker.book().expect("há livro").snapshot().asks
    }

    #[test]
    fn snapshot_builds_the_book_and_updates_move_it() {
        let mut tracker = Tracker::new(TESTE_BRL, Feed::L2);

        assert_eq!(
            tracker.handle(snapshot(4, vec![quote(10_000, 5)])),
            Action::Redraw
        );
        assert_eq!(tracker.handle(ask_level(5, 10_000, 3)), Action::Redraw);

        assert_eq!(asks(&tracker), vec![quote(10_000, 3)]);
    }

    #[test]
    fn updates_before_the_snapshot_are_ignored() {
        let mut tracker = Tracker::new(TESTE_BRL, Feed::L2);

        assert_eq!(tracker.handle(ask_level(1, 10_000, 3)), Action::Ignore);
        assert!(tracker.book().is_none());
    }

    #[test]
    fn gap_drops_the_book_and_asks_to_resubscribe() {
        let mut tracker = Tracker::new(TESTE_BRL, Feed::L2);
        tracker.handle(snapshot(0, vec![]));
        tracker.handle(ask_level(1, 10_000, 5));

        assert_eq!(
            tracker.handle(ask_level(3, 10_100, 2)),
            Action::Resubscribe(Gap {
                expected: 2,
                found: 3
            })
        );
        assert!(tracker.book().is_none());
        assert_eq!(tracker.handle(ask_level(4, 10_100, 1)), Action::Ignore);

        assert_eq!(
            tracker.handle(snapshot(4, vec![quote(10_000, 5), quote(10_100, 1)])),
            Action::Redraw
        );
        assert_eq!(asks(&tracker), vec![quote(10_000, 5), quote(10_100, 1)]);
    }

    #[test]
    fn other_streams_and_other_messages_are_ignored() {
        let mut tracker = Tracker::new(TESTE_BRL, Feed::L2);
        tracker.handle(snapshot(0, vec![]));

        let l1 = ServerMessage::from(&Update {
            instrument: TESTE_BRL,
            feed: Feed::L1,
            seq: 9,
            body: Body::Top {
                bid: None,
                ask: None,
            },
        });
        assert_eq!(tracker.handle(l1), Action::Ignore);
        assert_eq!(
            tracker.handle(ServerMessage::LoggedIn {
                account: exchange_core::domain::AccountId::new(1)
            }),
            Action::Ignore
        );
        assert_eq!(tracker.book().unwrap().seq(), 0);
    }

    #[test]
    fn server_errors_are_reported() {
        let mut tracker = Tracker::new(TESTE_BRL, Feed::L2);

        assert_eq!(
            tracker.handle(ServerMessage::Error {
                message: "o núcleo parou".to_string()
            }),
            Action::ServerError("o núcleo parou".to_string())
        );
    }

    #[test]
    fn server_messages_survive_the_round_trip() {
        let messages = [
            snapshot(4, vec![quote(10_000, 5)]),
            ask_level(5, 10_000, 3),
            ServerMessage::Error {
                message: "x".to_string(),
            },
            ServerMessage::LoggedIn {
                account: exchange_core::domain::AccountId::new(3),
            },
        ];
        for message in messages {
            let line = message.to_line();
            let back: ServerMessage = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(back, message, "{line}");
        }
    }

    #[test]
    fn render_draws_asks_above_bids_with_spread_and_last_trade() {
        let mut book = LocalBook::from_snapshot(&Snapshot {
            instrument: TESTE_BRL,
            feed: Feed::L2,
            seq: 7,
            bids: vec![quote(9_900, 3), quote(9_800, 1)],
            asks: vec![quote(10_000, 5), quote(10_100, 2)],
        });
        book.apply(&Update {
            instrument: TESTE_BRL,
            feed: Feed::L2,
            seq: 8,
            body: Body::Trade {
                price: Price::new(10_000).unwrap(),
                lots: Lots::new(2).unwrap(),
            },
        })
        .unwrap();

        assert_eq!(
            render(&book, 10),
            "TESTE/BRL  L2  seq 8\n\
             \x20       preço     lots\n\
             venda   10100        2\n\
             venda   10000        5\n\
             ------ spread 100 ------\n\
             compra   9900        3\n\
             compra   9800        1\n\
             último negócio: 2 lots @ 10000\n"
        );
    }

    /// Um servidor de mentira que manda linhas fixas e registra o que recebe: é a forma de
    /// testar o buraco de forma determinística, sem depender de um assinante lento de verdade.
    async fn scripted_server(
        script: Vec<Vec<ServerMessage>>,
    ) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut requests = BufReader::new(reader).lines();
            let mut received = Vec::new();
            for batch in script {
                let request = tokio::time::timeout(PATIENCE, requests.next_line())
                    .await
                    .expect("o cliente mandou o pedido a tempo")
                    .unwrap()
                    .expect("o cliente não fechou");
                received.push(request);
                let bytes: String = batch.iter().map(ServerMessage::to_line).collect();
                writer.write_all(bytes.as_bytes()).await.unwrap();
            }
            received
        });
        (address, server)
    }

    // O critério da F5, de ponta a ponta: o assinante percebe o buraco pela sequência, pede
    // outro snapshot sozinho e termina com o livro certo.
    #[tokio::test]
    async fn follow_detects_a_gap_resubscribes_and_recovers() {
        let first = vec![
            snapshot(0, vec![]),
            ask_level(1, 10_000, 5),
            ask_level(3, 10_100, 2),
        ];
        let second = vec![snapshot(3, vec![quote(10_000, 5), quote(10_100, 2)])];
        let (address, server) = scripted_server(vec![first, second]).await;

        let mut actions = Vec::new();
        let mut final_asks = None;
        let stream = TcpStream::connect(address).await.unwrap();
        tokio::time::timeout(
            PATIENCE,
            follow(stream, "TESTE/BRL", Feed::L2, |tracker, action| {
                actions.push(action.clone());
                final_asks = tracker.book().map(|book| book.snapshot().asks);
                if actions.len() == 4 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }),
        )
        .await
        .expect("o cliente seguiu a tempo")
        .unwrap();

        assert_eq!(
            actions,
            vec![
                Action::Redraw,
                Action::Redraw,
                Action::Resubscribe(Gap {
                    expected: 2,
                    found: 3
                }),
                Action::Redraw,
            ]
        );
        assert_eq!(final_asks, Some(vec![quote(10_000, 5), quote(10_100, 2)]));
        let subscribe = r#"{"feed":"l2","instrument":"TESTE/BRL","type":"subscribe"}"#;
        assert_eq!(
            server.await.unwrap(),
            vec![subscribe.to_string(), subscribe.to_string()]
        );
    }

    #[tokio::test]
    async fn follow_rejects_an_unknown_instrument() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();

        let error = follow(
            stream,
            "BTC/BRL",
            Feed::L2,
            |_, _| ControlFlow::Continue(()),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    // Contra o servidor de verdade: outro cliente negocia e o livro do assinante termina
    // igual ao snapshot do núcleo.
    #[tokio::test]
    async fn follow_tracks_the_real_server() {
        let (core, _thread) = spawn_core(
            Sequencer::in_memory(),
            new_market_data(),
            NonZeroUsize::new(16).unwrap(),
            NonZeroUsize::new(1024).unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let limits = Limits {
            max_connections: NonZeroUsize::new(8).unwrap(),
            idle_timeout: PATIENCE,
        };
        tokio::spawn(serve(listener, core.clone(), limits));

        let stream = TcpStream::connect(address).await.unwrap();
        let follower = tokio::spawn(async move {
            let mut last = None;
            follow(stream, "TESTE/BRL", Feed::L2, |tracker, _| {
                last = tracker.book().map(LocalBook::snapshot);
                match &last {
                    Some(book) if book.asks.len() == 2 && !book.bids.is_empty() => {
                        ControlFlow::Break(())
                    }
                    _ => ControlFlow::Continue(()),
                }
            })
            .await
            .unwrap();
            last
        });

        let trader = TcpStream::connect(address).await.unwrap();
        let (reader, mut writer) = trader.into_split();
        let mut replies = BufReader::new(reader).lines();
        for line in [
            r#"{"type":"login","account":1}"#,
            r#"{"type":"deposit","asset":"TESTE","amount":100000}"#,
            r#"{"type":"deposit","asset":"BRL","amount":10000000}"#,
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":5}"#,
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10100,"lots":2}"#,
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":9900,"lots":3}"#,
        ] {
            writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            replies.next_line().await.unwrap();
        }

        let seen = tokio::time::timeout(PATIENCE, follower)
            .await
            .expect("o assinante viu o livro a tempo")
            .unwrap()
            .expect("o assinante montou o livro");
        let truth = core.snapshot(TESTE_BRL, Feed::L2).await.unwrap().unwrap();
        assert_eq!(seen.bids, truth.bids);
        assert_eq!(seen.asks, truth.asks);
    }
}
