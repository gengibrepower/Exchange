use std::collections::VecDeque;

use proptest::prelude::*;

use super::*;
use crate::{Field, Frame, decode, encode};

const BASE_MILLIS: u64 = 1_760_000_000_000;

fn at(secs: u64) -> UtcTimestamp {
    UtcTimestamp::from_unix_millis(BASE_MILLIS + secs * 1_000).expect("instante de teste válido")
}

fn text(value: &str) -> Text {
    Text::new(value).expect("texto de teste válido")
}

fn seq(number: u64) -> SeqNum {
    SeqNum::new(number).expect("seq de teste não é zero")
}

fn secs(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("intervalo de teste não é zero")
}

fn config(role: Role) -> SessionConfig {
    let (sender, target) = match role {
        Role::Initiator => ("CLIENTE", "EXCHANGE"),
        Role::Acceptor => ("EXCHANGE", "CLIENTE"),
    };
    SessionConfig {
        role,
        sender: text(sender),
        target: text(target),
        heartbeat_secs: secs(30),
    }
}

fn app(id: &str) -> AppMessage {
    let field = Field::new(Tag::TEXT, id).expect("valor de teste válido");
    AppMessage::new(Message::new("D", vec![field]).expect("mensagem de teste válida"))
        .expect("D é de aplicação")
}

fn from_client(seq_number: u64, orig: Option<UtcTimestamp>, body: Body) -> Message {
    let header = Header {
        sender: text("CLIENTE"),
        target: text("EXCHANGE"),
        seq: seq(seq_number),
        sending_time: at(0),
        orig_sending_time: orig,
    };
    Envelope { header, body }.to_message()
}

fn client_admin(seq_number: u64, admin: Admin) -> Message {
    from_client(seq_number, None, Body::Admin(admin))
}

fn logon(seq_number: u64, heartbeat: u32) -> Message {
    client_admin(
        seq_number,
        Admin::Logon {
            heartbeat_secs: secs(heartbeat),
        },
    )
}

fn sent(actions: &[Action]) -> Vec<Envelope> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Send(message) => {
                Some(Envelope::parse(message.clone()).expect("a sessão só envia mensagens válidas"))
            }
            Action::Deliver(_) | Action::Disconnect => None,
        })
        .collect()
}

fn bodies(actions: &[Action]) -> Vec<Body> {
    sent(actions)
        .into_iter()
        .map(|envelope| envelope.body)
        .collect()
}

fn disconnects(actions: &[Action]) -> bool {
    actions.last() == Some(&Action::Disconnect)
}

fn is_logout(body: &Body) -> bool {
    matches!(body, Body::Admin(Admin::Logout { .. }))
}

/// Acceptor que já respondeu o Logon do cliente: enviou o seq 1 e espera o 2.
fn active_acceptor() -> Session {
    let mut session = Session::new(config(Role::Acceptor));
    session.connect(at(0));
    session.receive(at(0), logon(1, 30));
    assert!(session.is_active());
    session
}

#[test]
fn initiator_opens_with_logon_and_acceptor_waits() {
    let mut initiator = Session::new(config(Role::Initiator));
    assert_eq!(
        bodies(&initiator.connect(at(0))),
        vec![Body::Admin(Admin::Logon {
            heartbeat_secs: secs(30)
        })]
    );
    let mut acceptor = Session::new(config(Role::Acceptor));
    assert_eq!(acceptor.connect(at(0)), vec![]);
}

#[test]
fn acceptor_answers_logon_adopting_the_clients_heartbeat() {
    let mut session = Session::new(config(Role::Acceptor));
    session.connect(at(0));
    let actions = session.receive(at(0), logon(1, 10));
    let answers = sent(&actions);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].header.sender, text("EXCHANGE"));
    assert_eq!(answers[0].header.target, text("CLIENTE"));
    assert_eq!(
        answers[0].body,
        Body::Admin(Admin::Logon {
            heartbeat_secs: secs(10)
        })
    );
    assert!(session.is_active());
    assert_eq!(session.next_inbound(), seq(2));
}

#[test]
fn first_message_other_than_logon_drops_the_connection() {
    let mut session = Session::new(config(Role::Acceptor));
    session.connect(at(0));
    let actions = session.receive(
        at(0),
        client_admin(1, Admin::Heartbeat { test_req_id: None }),
    );
    assert_eq!(actions, vec![Action::Disconnect]);
    assert!(!session.is_active());
}

#[test]
fn unexpected_comp_id_logs_out() {
    let mut session = Session::new(config(Role::Acceptor));
    session.connect(at(0));
    let mut message = logon(1, 30);
    let header = Header {
        sender: text("OUTRO"),
        ..Envelope::parse(message.clone())
            .expect("logon válido")
            .header
    };
    message = Envelope {
        header,
        body: Body::Admin(Admin::Logon {
            heartbeat_secs: secs(30),
        }),
    }
    .to_message();
    let actions = session.receive(at(0), message);
    assert!(is_logout(&bodies(&actions)[0]));
    assert!(disconnects(&actions));
}

#[test]
fn in_sequence_app_message_is_delivered() {
    let mut session = active_acceptor();
    let actions = session.receive(at(1), from_client(2, None, Body::App(app("a"))));
    assert_eq!(actions, vec![Action::Deliver(app("a"))]);
    assert_eq!(session.next_inbound(), seq(3));
}

#[test]
fn gap_asks_for_resend_once_and_discards_until_filled() {
    let mut session = active_acceptor();
    let actions = session.receive(at(1), from_client(5, None, Body::App(app("e"))));
    assert_eq!(
        bodies(&actions),
        vec![Body::Admin(Admin::ResendRequest {
            begin: seq(2),
            end: None
        })]
    );
    assert_eq!(
        session.receive(at(1), from_client(6, None, Body::App(app("f")))),
        vec![]
    );
    assert_eq!(session.next_inbound(), seq(2));

    let fill = client_admin(
        2,
        Admin::SequenceReset {
            gap_fill: true,
            new_seq: seq(5),
        },
    );
    assert_eq!(session.receive(at(2), fill), vec![]);
    let replayed = session.receive(at(2), from_client(5, Some(at(1)), Body::App(app("e"))));
    assert_eq!(replayed, vec![Action::Deliver(app("e"))]);
    let replayed = session.receive(at(2), from_client(6, Some(at(1)), Body::App(app("f"))));
    assert_eq!(replayed, vec![Action::Deliver(app("f"))]);

    let next_gap = session.receive(at(3), from_client(8, None, Body::App(app("h"))));
    assert_eq!(
        bodies(&next_gap),
        vec![Body::Admin(Admin::ResendRequest {
            begin: seq(7),
            end: None
        })],
        "com o primeiro buraco fechado, um novo buraco pede resend de novo"
    );
}

#[test]
fn logon_ahead_of_expected_is_accepted_then_asks_for_resend() {
    let mut session = Session::new(config(Role::Acceptor));
    session.connect(at(0));
    let actions = session.receive(at(0), logon(4, 30));
    assert!(session.is_active());
    assert_eq!(
        bodies(&actions),
        vec![
            Body::Admin(Admin::Logon {
                heartbeat_secs: secs(30)
            }),
            Body::Admin(Admin::ResendRequest {
                begin: seq(1),
                end: None
            }),
        ]
    );
}

#[test]
fn possible_duplicate_below_expected_is_ignored() {
    let mut session = active_acceptor();
    session.receive(at(1), from_client(2, None, Body::App(app("a"))));
    let again = session.receive(at(2), from_client(2, Some(at(1)), Body::App(app("a"))));
    assert_eq!(again, vec![]);
    assert_eq!(session.next_inbound(), seq(3));
}

#[test]
fn below_expected_without_possible_duplicate_logs_out() {
    let mut session = active_acceptor();
    session.receive(at(1), from_client(2, None, Body::App(app("a"))));
    let actions = session.receive(at(2), from_client(2, None, Body::App(app("a"))));
    assert!(is_logout(&bodies(&actions)[0]));
    assert!(disconnects(&actions));
}

#[test]
fn resend_replays_app_messages_and_gap_fills_admin_ones() {
    let mut session = active_acceptor();
    session.send(at(1), app("a"));
    session.tick(at(31));
    session.send(at(32), app("b"));
    assert_eq!(session.next_outbound(), seq(5));

    let request = client_admin(
        2,
        Admin::ResendRequest {
            begin: seq(1),
            end: None,
        },
    );
    let replay = sent(&session.receive(at(40), request));
    let shape: Vec<(u64, Body)> = replay
        .iter()
        .map(|envelope| (envelope.header.seq.get(), envelope.body.clone()))
        .collect();
    let fill = |new_seq| {
        Body::Admin(Admin::SequenceReset {
            gap_fill: true,
            new_seq: seq(new_seq),
        })
    };
    assert_eq!(
        shape,
        vec![
            (1, fill(2)),
            (2, Body::App(app("a"))),
            (3, fill(4)),
            (4, Body::App(app("b"))),
        ]
    );
    assert!(
        replay
            .iter()
            .all(|envelope| envelope.header.orig_sending_time.is_some()),
        "todo reenvio leva PossDupFlag=Y"
    );
    assert_eq!(replay[1].header.orig_sending_time, Some(at(1)));
    assert_eq!(
        session.next_outbound(),
        seq(5),
        "reenviar não consome seq novo"
    );
}

#[test]
fn resend_ending_in_admin_messages_fills_up_to_the_next_seq() {
    let mut session = active_acceptor();
    session.send(at(1), app("a"));
    session.tick(at(31));
    let request = client_admin(
        2,
        Admin::ResendRequest {
            begin: seq(2),
            end: Some(seq(99)),
        },
    );
    let replay = sent(&session.receive(at(40), request));
    assert_eq!(replay.len(), 2);
    assert_eq!(
        replay[1].body,
        Body::Admin(Admin::SequenceReset {
            gap_fill: true,
            new_seq: seq(4)
        })
    );
}

#[test]
fn gap_fill_backwards_is_rejected() {
    let mut session = active_acceptor();
    session.receive(at(1), from_client(2, None, Body::App(app("a"))));
    let backwards = client_admin(
        3,
        Admin::SequenceReset {
            gap_fill: true,
            new_seq: seq(2),
        },
    );
    let actions = session.receive(at(2), backwards);
    assert_eq!(
        bodies(&actions),
        vec![Body::Admin(Admin::Reject {
            ref_seq: seq(3),
            ref_tag: Some(Tag::NEW_SEQ_NO),
            reason: Some(RejectReason::ValueIncorrect),
            text: None
        })]
    );
}

#[test]
fn reset_mode_ignores_the_message_seq() {
    let mut session = active_acceptor();
    let reset = client_admin(
        40,
        Admin::SequenceReset {
            gap_fill: false,
            new_seq: seq(50),
        },
    );
    assert_eq!(session.receive(at(1), reset), vec![]);
    assert_eq!(session.next_inbound(), seq(50));
}

#[test]
fn test_request_is_answered_with_its_id() {
    let mut session = active_acceptor();
    let request = client_admin(
        2,
        Admin::TestRequest {
            test_req_id: text("ping"),
        },
    );
    assert_eq!(
        bodies(&session.receive(at(1), request)),
        vec![Body::Admin(Admin::Heartbeat {
            test_req_id: Some(text("ping"))
        })]
    );
}

#[test]
fn heartbeat_goes_out_after_an_interval_without_sending() {
    let mut session = active_acceptor();
    session.receive(
        at(25),
        client_admin(2, Admin::Heartbeat { test_req_id: None }),
    );
    assert_eq!(session.tick(at(29)), vec![]);
    assert_eq!(
        bodies(&session.tick(at(30))),
        vec![Body::Admin(Admin::Heartbeat { test_req_id: None })]
    );
}

#[test]
fn silence_triggers_test_request_and_then_disconnect() {
    let mut session = active_acceptor();
    let probe = bodies(&session.tick(at(36)));
    assert_eq!(
        probe,
        vec![Body::Admin(Admin::TestRequest {
            test_req_id: text("TEST-1")
        })]
    );
    assert_eq!(session.tick(at(50)), vec![]);
    let actions = session.tick(at(66));
    assert!(is_logout(&bodies(&actions)[0]));
    assert!(disconnects(&actions));
}

#[test]
fn any_traffic_answers_a_pending_test_request() {
    let mut session = active_acceptor();
    session.tick(at(36));
    session.receive(
        at(40),
        client_admin(2, Admin::Heartbeat { test_req_id: None }),
    );
    assert!(!disconnects(&session.tick(at(66))));
}

#[test]
fn logon_never_arriving_times_out() {
    let mut session = Session::new(config(Role::Acceptor));
    session.connect(at(0));
    assert_eq!(session.tick(at(29)), vec![]);
    assert_eq!(session.tick(at(30)), vec![Action::Disconnect]);
}

#[test]
fn messages_sent_while_offline_go_out_right_after_logon() {
    let mut session = Session::new(config(Role::Acceptor));
    assert_eq!(session.send(at(0), app("cedo")), vec![]);
    session.connect(at(1));
    let actions = session.receive(at(1), logon(1, 30));
    let sent = sent(&actions);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].header.seq, seq(2));
    assert_eq!(sent[1].body, Body::App(app("cedo")));
}

#[test]
fn logout_is_answered_and_disconnects() {
    let mut session = active_acceptor();
    let actions = session.receive(at(1), client_admin(2, Admin::Logout { text: None }));
    assert!(is_logout(&bodies(&actions)[0]));
    assert!(disconnects(&actions));
}

#[test]
fn our_logout_waits_for_the_answer() {
    let mut session = active_acceptor();
    assert!(is_logout(&bodies(&session.logout(at(1)))[0]));
    let actions = session.receive(at(2), client_admin(2, Admin::Logout { text: None }));
    assert_eq!(actions, vec![Action::Disconnect]);
}

#[test]
fn malformed_message_in_sequence_is_rejected_and_consumes_the_seq() {
    let mut session = active_acceptor();
    let field = |tag: Tag, value: &str| Field::new(tag, value).expect("campo de teste válido");
    let without_sending_time = Message::new(
        "0",
        vec![
            field(Tag::SENDER_COMP_ID, "CLIENTE"),
            field(Tag::TARGET_COMP_ID, "EXCHANGE"),
            field(Tag::MSG_SEQ_NUM, "2"),
        ],
    )
    .expect("mensagem de teste válida");
    let actions = session.receive(at(1), without_sending_time);
    assert_eq!(
        bodies(&actions),
        vec![Body::Admin(Admin::Reject {
            ref_seq: seq(2),
            ref_tag: Some(Tag::SENDING_TIME),
            reason: Some(RejectReason::RequiredTagMissing),
            text: None
        })]
    );
    assert_eq!(session.next_inbound(), seq(3));
}

#[test]
fn nothing_happens_while_disconnected() {
    let mut session = active_acceptor();
    session.disconnected();
    assert_eq!(
        session.receive(at(1), from_client(2, None, Body::App(app("a")))),
        vec![]
    );
    assert_eq!(session.tick(at(100)), vec![]);
    assert_eq!(session.next_inbound(), seq(2));
}

const INITIATOR: usize = 0;
const ACCEPTOR: usize = 1;

#[derive(Clone, Copy, Debug)]
enum Op {
    Send(usize),
    Deliver(usize),
    Tick(u64),
    Duplicate(usize),
    Drop,
    Connect,
}

struct Peer {
    session: Session,
    sent: Vec<AppMessage>,
    delivered: Vec<AppMessage>,
}

/// Duas sessões ligadas por um "TCP" de brinquedo: cada sentido é uma fila FIFO, e uma
/// queda perde tudo que estava em trânsito nos dois sentidos, como uma conexão real.
/// `Duplicate` faz o papel de uma contraparte que reenvia mais do que foi pedido, dentro
/// da conexão atual e depois do Logon, que é onde um reenvio pode acontecer.
struct Link2 {
    now: u64,
    up: bool,
    peers: [Peer; 2],
    in_flight: [VecDeque<Message>; 2],
    last_delivered: [Option<Message>; 2],
}

impl Link2 {
    fn new() -> Self {
        let peer = |role| Peer {
            session: Session::new(config(role)),
            sent: Vec::new(),
            delivered: Vec::new(),
        };
        Self {
            now: BASE_MILLIS,
            up: false,
            peers: [peer(Role::Initiator), peer(Role::Acceptor)],
            in_flight: [VecDeque::new(), VecDeque::new()],
            last_delivered: [None, None],
        }
    }

    fn now(&self) -> UtcTimestamp {
        UtcTimestamp::from_unix_millis(self.now).expect("simulação fica no intervalo válido")
    }

    fn apply(&mut self, side: usize, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(message) if self.up => self.in_flight[side].push_back(message),
                Action::Send(_) => {}
                Action::Deliver(app) => self.peers[side].delivered.push(app),
                Action::Disconnect => self.drop_link(),
            }
        }
    }

    fn drop_link(&mut self) {
        self.up = false;
        for side in [INITIATOR, ACCEPTOR] {
            self.in_flight[side].clear();
            self.last_delivered[side] = None;
            self.peers[side].session.disconnected();
        }
    }

    fn run(&mut self, op: Op) {
        let now = self.now();
        match op {
            Op::Send(side) => {
                let app = app(&format!("{side}-{}", self.peers[side].sent.len()));
                self.peers[side].sent.push(app.clone());
                let actions = self.peers[side].session.send(now, app);
                self.apply(side, actions);
            }
            Op::Deliver(side) => {
                let Some(message) = self.in_flight[side].pop_front() else {
                    return;
                };
                let message = over_the_wire(&message);
                self.last_delivered[side] = Some(message.clone());
                let actions = self.peers[1 - side].session.receive(now, message);
                self.apply(1 - side, actions);
            }
            Op::Tick(millis) => {
                self.now += millis;
                let now = self.now();
                for side in [INITIATOR, ACCEPTOR] {
                    let actions = self.peers[side].session.tick(now);
                    self.apply(side, actions);
                }
            }
            Op::Duplicate(side) => {
                let receiver_active = self.peers[1 - side].session.is_active();
                if let (true, Some(original)) = (receiver_active, &self.last_delivered[side]) {
                    let mut copy = Envelope::parse(original.clone()).expect("já foi aceita");
                    copy.header.orig_sending_time = Some(copy.header.sending_time);
                    self.in_flight[side].push_back(copy.to_message());
                }
            }
            Op::Drop => {
                if self.up {
                    self.drop_link();
                }
            }
            Op::Connect => {
                if !self.up {
                    self.up = true;
                    for side in [ACCEPTOR, INITIATOR] {
                        let actions = self.peers[side].session.connect(now);
                        self.apply(side, actions);
                    }
                }
            }
        }
    }

    /// Uma reconexão limpa, sem quedas nem tempo passando, entregando tudo até o silêncio.
    fn heal(&mut self) {
        self.run(Op::Connect);
        while !(self.in_flight[INITIATOR].is_empty() && self.in_flight[ACCEPTOR].is_empty()) {
            self.run(Op::Deliver(INITIATOR));
            self.run(Op::Deliver(ACCEPTOR));
        }
    }
}

fn over_the_wire(message: &Message) -> Message {
    let mut bytes = Vec::new();
    encode(message, &mut bytes).expect("mensagens da simulação cabem no teto");
    match decode(&bytes) {
        Frame::Complete { message, .. } => message,
        other => panic!("quadro válido não decodificou: {other:?}"),
    }
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Send(INITIATOR)),
        3 => Just(Op::Send(ACCEPTOR)),
        8 => Just(Op::Deliver(INITIATOR)),
        8 => Just(Op::Deliver(ACCEPTOR)),
        2 => (0..45_000u64).prop_map(Op::Tick),
        2 => Just(Op::Duplicate(INITIATOR)),
        2 => Just(Op::Duplicate(ACCEPTOR)),
        1 => Just(Op::Drop),
        2 => Just(Op::Connect),
    ]
}

proptest! {
    /// O critério de pronto da F6 como propriedade: com quedas, mensagens perdidas em
    /// trânsito, reenvios a mais, timeouts e envios offline em qualquer ordem, depois de uma reconexão
    /// limpa cada lado recebeu exatamente o que o outro enviou, uma vez e em ordem.
    #[test]
    fn every_app_message_arrives_exactly_once_and_in_order(ops in prop::collection::vec(op(), 0..200)) {
        let mut link = Link2::new();
        for op in ops {
            link.run(op);
        }
        link.heal();
        prop_assert!(link.up, "a reconexão limpa não pode cair");
        prop_assert_eq!(&link.peers[ACCEPTOR].delivered, &link.peers[INITIATOR].sent);
        prop_assert_eq!(&link.peers[INITIATOR].delivered, &link.peers[ACCEPTOR].sent);
    }
}
