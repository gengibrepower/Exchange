use std::net::SocketAddr;
use std::num::NonZeroUsize;

use exchange_core::domain::Amount;
use exchange_fix::{AppMessage, Field, Tag};

use super::*;
use crate::core_thread::spawn_core;
use crate::market::{BRL, INSTRUMENTS, TESTE, new_market_data};
use crate::market_data::Feed;
use crate::sequencer::{Intent, Sequencer};

const PATIENCE: Duration = Duration::from_secs(5);

async fn start() -> (SocketAddr, CoreHandle) {
    let (core, _thread) = spawn_core(
        Sequencer::in_memory(),
        new_market_data(),
        NonZeroUsize::new(16).unwrap(),
        NonZeroUsize::new(1024).unwrap(),
        NonZeroUsize::new(1024).unwrap(),
    )
    .unwrap();
    for account in [1, 2] {
        for asset in [TESTE, BRL] {
            let deposit = Intent::Deposit {
                account: AccountId::new(account),
                asset,
                amount: Amount::new(10_000_000).unwrap(),
            };
            core.submit(deposit).await.unwrap();
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let limits = Limits {
        max_connections: NonZeroUsize::new(64).unwrap(),
        idle_timeout: PATIENCE,
    };
    let registry = Arc::new(Registry::default());
    tokio::spawn(serve(listener, core.clone(), registry, limits));
    (address, core)
}

/// Um cliente FIX de verdade: a mesma [`Session`] do servidor, no papel de initiator, sobre
/// um socket. O estado da sessão sobrevive a [`Client::crash`], como num cliente que
/// guarda seus seqs e reconecta.
struct Client {
    session: Session,
    stream: Option<TcpStream>,
    inbox: Vec<u8>,
    delivered: Vec<AppMessage>,
}

impl Client {
    fn new(sender: &str, target: &str) -> Self {
        Self {
            session: Session::new(SessionConfig {
                role: Role::Initiator,
                sender: Text::new(sender).unwrap(),
                target: Text::new(target).unwrap(),
                heartbeat_secs: NonZeroU32::new(30).unwrap(),
            }),
            stream: None,
            inbox: Vec::new(),
            delivered: Vec::new(),
        }
    }

    fn account(number: u64) -> Self {
        Self::new(&number.to_string(), EXCHANGE_COMP_ID)
    }

    /// Uma tentativa de logon; `false` se o servidor fechou a conexão.
    async fn try_log_on(&mut self, address: SocketAddr) -> bool {
        self.stream = Some(TcpStream::connect(address).await.unwrap());
        self.inbox.clear();
        let actions = self.session.connect(now());
        self.perform(actions).await;
        while !self.session.is_active() {
            if !self.pump().await {
                return false;
            }
        }
        true
    }

    /// Logon insistente: logo depois de uma queda, o servidor pode ainda não ter notado que
    /// a conexão anterior morreu e recusar a nova por já haver uma ativa.
    async fn log_on(&mut self, address: SocketAddr) {
        for _ in 0..100 {
            if self.try_log_on(address).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("o logon não foi aceito");
    }

    async fn send(&mut self, app: AppMessage) {
        let actions = self.session.send(now(), app);
        self.perform(actions).await;
    }

    /// Cai sem ler o que o servidor já mandou: o que estava em trânsito se perde.
    fn crash(&mut self) {
        self.stream = None;
        self.session.disconnected();
    }

    async fn perform(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(message) => {
                    let mut bytes = Vec::new();
                    encode(&message, &mut bytes).unwrap();
                    if let Some(stream) = &mut self.stream {
                        stream.write_all(&bytes).await.unwrap();
                    }
                }
                Action::Deliver { app, .. } => self.delivered.push(app),
                Action::Disconnect => self.crash(),
            }
        }
    }

    /// Lê um pedaço e o entrega à sessão; `false` se a conexão fechou.
    async fn pump(&mut self) -> bool {
        let Some(stream) = &mut self.stream else {
            return false;
        };
        let mut chunk = [0; READ_CHUNK];
        let read = tokio::time::timeout(PATIENCE, stream.read(&mut chunk))
            .await
            .expect("o servidor respondeu a tempo");
        match read {
            Ok(0) | Err(_) => {
                self.crash();
                false
            }
            Ok(read) => {
                self.inbox.extend_from_slice(&chunk[..read]);
                while let Some(message) = take_frame(&mut self.inbox) {
                    let actions = self.session.receive(now(), message);
                    self.perform(actions).await;
                }
                true
            }
        }
    }

    async fn wait_for_deliveries(&mut self, count: usize) {
        while self.delivered.len() < count {
            assert!(self.pump().await, "o servidor fechou a conexão");
        }
    }
}

fn order(cl_ord_id: &str, side: &str, lots: &str, price: &str) -> AppMessage {
    let fields = [
        (11, cl_ord_id),
        (55, "TESTE/BRL"),
        (54, side),
        (38, lots),
        (40, "2"),
        (44, price),
    ]
    .iter()
    .map(|&(number, value)| Field::new(Tag::new(number).unwrap(), value).unwrap())
    .collect();
    AppMessage::new(Message::new("D", fields).unwrap()).unwrap()
}

fn get(app: &AppMessage, number: u32) -> Option<&str> {
    app.message()
        .body()
        .iter()
        .find(|field| field.tag() == Tag::new(number).unwrap())
        .map(|field| std::str::from_utf8(field.value()).unwrap())
}

async fn resting_bid_lots(core: &CoreHandle) -> i64 {
    let snapshot = core
        .snapshot(INSTRUMENTS[0].1, Feed::L2)
        .await
        .unwrap()
        .unwrap();
    snapshot.bids.iter().map(|quote| quote.lots.get()).sum()
}

async fn wait_until_resting(core: &CoreHandle, lots: i64) {
    tokio::time::timeout(PATIENCE, async {
        while resting_bid_lots(core).await != lots {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a ordem chegou ao livro a tempo");
}

#[tokio::test]
async fn order_over_fix_is_acknowledged() {
    let (address, _core) = start().await;
    let mut client = Client::account(1);
    client.log_on(address).await;
    client.send(order("v1", "2", "5", "1000")).await;
    client.wait_for_deliveries(1).await;
    let report = &client.delivered[0];
    assert_eq!(report.message().msg_type(), b"8");
    assert_eq!(get(report, 11), Some("v1"));
    assert_eq!(get(report, 150), Some("0"));
}

/// O critério de pronto da F6: a conexão cai depois de a ordem chegar ao núcleo e antes de
/// o cliente ler a confirmação. Ao reconectar, o Logon do servidor revela o buraco, o
/// resend entrega a confirmação perdida uma vez só, e o cliente que, na dúvida, manda a
/// mesma ordem de novo recebe uma recusa por `ClOrdID` em vez de uma segunda ordem.
#[tokio::test]
async fn reconnect_recovers_the_lost_report_without_duplicating_the_order() {
    let (address, core) = start().await;
    let mut client = Client::account(2);
    client.log_on(address).await;
    client.send(order("c1", "1", "5", "1000")).await;
    wait_until_resting(&core, 5).await;
    client.crash();
    assert!(client.delivered.is_empty(), "a confirmação ficou no fio");

    client.log_on(address).await;
    client.wait_for_deliveries(1).await;
    let recovered = &client.delivered[0];
    assert_eq!(get(recovered, 11), Some("c1"));
    assert_eq!(get(recovered, 150), Some("0"));

    client.send(order("c1", "1", "5", "1000")).await;
    client.wait_for_deliveries(2).await;
    let retry = &client.delivered[1];
    assert_eq!(get(retry, 150), Some("8"));
    assert_eq!(get(retry, 103), Some("6"));

    assert_eq!(client.delivered.len(), 2, "nenhuma confirmação duplicada");
    assert_eq!(resting_bid_lots(&core).await, 5, "uma ordem só no livro");
}

#[tokio::test]
async fn second_connection_for_the_same_account_is_refused() {
    let (address, _core) = start().await;
    let mut first = Client::account(1);
    first.log_on(address).await;
    let mut second = Client::account(1);
    assert!(!second.try_log_on(address).await);
    first.send(order("v1", "2", "1", "1000")).await;
    first.wait_for_deliveries(1).await;
}

#[tokio::test]
async fn house_account_and_wrong_target_are_refused() {
    let (address, _core) = start().await;
    assert!(!Client::account(0).try_log_on(address).await);
    assert!(!Client::new("1", "OUTRA").try_log_on(address).await);
    assert!(
        !Client::new("01", EXCHANGE_COMP_ID)
            .try_log_on(address)
            .await
    );
}

#[tokio::test]
async fn garbage_before_the_logon_is_skipped() {
    let (address, _core) = start().await;
    let mut client = Client::account(1);
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"lixo\x01qualquer").await.unwrap();
    client.stream = Some(stream);
    let actions = client.session.connect(now());
    client.perform(actions).await;
    while !client.session.is_active() {
        assert!(client.pump().await, "o servidor fechou a conexão");
    }
}
