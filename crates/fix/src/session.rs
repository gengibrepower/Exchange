use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU32;

use crate::{
    Admin, AppMessage, Body, Envelope, Header, Message, ParseError, RejectReason, SeqNum,
    SessionReject, Tag, Text, UtcTimestamp,
};

/// Quem abre: o initiator manda o Logon ao conectar, o acceptor espera e responde. Fora
/// isso as duas pontas seguem exatamente as mesmas regras.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Initiator,
    Acceptor,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionConfig {
    pub role: Role,
    pub sender: Text,
    pub target: Text,
    /// O initiator propõe este intervalo no Logon e o acceptor adota o que vier. Também é
    /// o prazo para o Logon chegar.
    pub heartbeat_secs: NonZeroU32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Send(Message),
    /// O `seq` acompanha a mensagem para que a aplicação possa recusá-la citando qual foi
    /// (`RefSeqNum` do `BusinessMessageReject`).
    Deliver {
        seq: SeqNum,
        app: AppMessage,
    },
    Disconnect,
}

/// Tudo que foi transmitido fica guardado pelo seq, para atender `ResendRequest`: a de
/// aplicação volta inteira, a administrativa vira `SequenceReset-GapFill`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Sent {
    time: UtcTimestamp,
    app: Option<AppMessage>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Link {
    Disconnected,
    AwaitingLogon { since: UtcTimestamp },
    Active(Active),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Active {
    heartbeat_ms: u64,
    last_sent: UtcTimestamp,
    last_received: UtcTimestamp,
    test_request_sent: Option<UtcTimestamp>,
    /// O maior seq visto além do buraco desde o último `ResendRequest`. Enquanto o esperado
    /// não passar dele, um novo seq alto só confirma o buraco já pedido.
    recovering_through: Option<SeqNum>,
}

/// Uma sessão FIX pura: não lê relógio nem toca rede. Cada entrada recebe o instante como
/// dado (decisão 8) e devolve as ações que a borda deve executar, na ordem. Os seqs e as
/// mensagens enviadas sobrevivem a uma queda de conexão; o estado da conexão, não.
#[derive(Clone, Debug)]
pub struct Session {
    config: SessionConfig,
    next_outbound: SeqNum,
    next_inbound: SeqNum,
    sent: BTreeMap<SeqNum, Sent>,
    pending: VecDeque<AppMessage>,
    link: Link,
    test_requests: u64,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            next_outbound: SeqNum::FIRST,
            next_inbound: SeqNum::FIRST,
            sent: BTreeMap::new(),
            pending: VecDeque::new(),
            link: Link::Disconnected,
            test_requests: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.link, Link::Active(_))
    }

    pub fn next_inbound(&self) -> SeqNum {
        self.next_inbound
    }

    pub fn next_outbound(&self) -> SeqNum {
        self.next_outbound
    }

    pub fn connect(&mut self, now: UtcTimestamp) -> Vec<Action> {
        let mut out = Vec::new();
        self.link = Link::AwaitingLogon { since: now };
        if self.config.role == Role::Initiator {
            let heartbeat_secs = self.config.heartbeat_secs;
            self.transmit(now, Body::Admin(Admin::Logon { heartbeat_secs }), &mut out);
        }
        out
    }

    /// A borda avisa que a conexão caiu. O que estava em trânsito se perdeu, mas os seqs
    /// guardados fazem o próximo Logon revelar o buraco e o resend recuperá-lo.
    pub fn disconnected(&mut self) {
        self.link = Link::Disconnected;
    }

    /// O seq só é atribuído quando a mensagem vai para o fio. Sem sessão ativa ela espera
    /// na fila e sai logo depois do Logon.
    pub fn send(&mut self, now: UtcTimestamp, app: AppMessage) -> Vec<Action> {
        let mut out = Vec::new();
        self.pending.push_back(app);
        if self.is_active() {
            self.flush_pending(now, &mut out);
        }
        out
    }

    pub fn receive(&mut self, now: UtcTimestamp, message: Message) -> Vec<Action> {
        let mut out = Vec::new();
        if let Link::Active(active) = &mut self.link {
            active.last_received = now;
            active.test_request_sent = None;
        }
        match (self.link, Envelope::parse(message)) {
            (Link::Disconnected, _) => {}
            (Link::AwaitingLogon { .. }, Err(_)) => self.drop_link(&mut out),
            (_, Err(ParseError::Unsequenced)) => {
                self.terminate(now, "MsgSeqNum ausente ou ilegível", &mut out);
            }
            (_, Err(ParseError::Reject(reject))) => self.on_malformed(now, reject, &mut out),
            (_, Ok(envelope)) if !self.is_from_counterparty(&envelope.header) => {
                self.terminate(now, "CompID inesperado", &mut out);
            }
            (Link::AwaitingLogon { .. }, Ok(envelope)) => self.on_logon(now, &envelope, &mut out),
            (Link::Active(_), Ok(envelope)) => {
                self.on_sequenced(now, envelope, &mut out);
            }
        }
        out
    }

    pub fn tick(&mut self, now: UtcTimestamp) -> Vec<Action> {
        let mut out = Vec::new();
        match self.link {
            Link::Disconnected => {}
            Link::AwaitingLogon { since } => {
                let timeout = u64::from(self.config.heartbeat_secs.get()) * 1_000;
                if now.saturating_millis_since(since) >= timeout {
                    self.drop_link(&mut out);
                }
            }
            Link::Active(_) => self.keep_alive(now, &mut out),
        }
        out
    }

    fn on_logon(&mut self, now: UtcTimestamp, envelope: &Envelope, out: &mut Vec<Action>) {
        let Body::Admin(Admin::Logon { heartbeat_secs }) = &envelope.body else {
            return self.drop_link(out);
        };
        if envelope.header.seq < self.next_inbound {
            return self.terminate(now, "MsgSeqNum abaixo do esperado", out);
        }
        let heartbeat_secs = match self.config.role {
            Role::Acceptor => *heartbeat_secs,
            Role::Initiator => self.config.heartbeat_secs,
        };
        self.link = Link::Active(Active {
            heartbeat_ms: u64::from(heartbeat_secs.get()) * 1_000,
            last_sent: now,
            last_received: now,
            test_request_sent: None,
            recovering_through: None,
        });
        if self.config.role == Role::Acceptor {
            self.transmit(now, Body::Admin(Admin::Logon { heartbeat_secs }), out);
        }
        if envelope.header.seq == self.next_inbound {
            self.next_inbound = following(envelope.header.seq);
        } else {
            self.request_resend(now, envelope.header.seq, out);
        }
        self.flush_pending(now, out);
    }

    fn on_sequenced(&mut self, now: UtcTimestamp, envelope: Envelope, out: &mut Vec<Action>) {
        let Envelope { header, body } = envelope;
        if let Body::Admin(Admin::SequenceReset {
            gap_fill: false,
            new_seq,
        }) = body
        {
            return self.jump_inbound(now, header.seq, new_seq, out);
        }
        match header.seq.cmp(&self.next_inbound) {
            Ordering::Less => {
                if header.orig_sending_time.is_none() {
                    self.terminate(now, "MsgSeqNum abaixo do esperado", out);
                }
            }
            Ordering::Greater => {
                if let Body::Admin(Admin::ResendRequest { begin, end }) = body {
                    self.resend(now, begin, end, out);
                }
                self.request_resend(now, header.seq, out);
            }
            Ordering::Equal => {
                self.next_inbound = following(header.seq);
                self.on_in_sequence(now, header.seq, body, out);
                self.finish_recovery();
            }
        }
    }

    fn on_in_sequence(
        &mut self,
        now: UtcTimestamp,
        seq: SeqNum,
        body: Body,
        out: &mut Vec<Action>,
    ) {
        let admin = match body {
            Body::App(app) => return out.push(Action::Deliver { seq, app }),
            Body::Admin(admin) => admin,
        };
        match admin {
            Admin::Heartbeat { .. } | Admin::Reject { .. } => {}
            Admin::TestRequest { test_req_id } => {
                let heartbeat = Admin::Heartbeat {
                    test_req_id: Some(test_req_id),
                };
                self.transmit(now, Body::Admin(heartbeat), out);
            }
            Admin::ResendRequest { begin, end } => self.resend(now, begin, end, out),
            Admin::SequenceReset { new_seq, .. } => self.jump_inbound(now, seq, new_seq, out),
            Admin::Logon { .. } => self.terminate(now, "Logon com a sessão já ativa", out),
            Admin::Logout { .. } => {
                self.transmit(now, Body::Admin(Admin::Logout { text: None }), out);
                self.drop_link(out);
            }
        }
    }

    /// Sem o header legível não há como saber se era duplicata: abaixo do esperado, a
    /// mensagem é só descartada; no esperado, recusada com Reject e o seq é consumido.
    fn on_malformed(&mut self, now: UtcTimestamp, reject: SessionReject, out: &mut Vec<Action>) {
        match reject.ref_seq.cmp(&self.next_inbound) {
            Ordering::Less => {}
            Ordering::Greater => self.request_resend(now, reject.ref_seq, out),
            Ordering::Equal => {
                self.next_inbound = following(reject.ref_seq);
                self.send_reject(now, reject, out);
                self.finish_recovery();
            }
        }
    }

    /// `SequenceReset`: no modo `GapFill` chega em sequência e o esperado já avançou um; no
    /// modo Reset o seq da mensagem nem conta. Nos dois, voltar o esperado é proibido.
    fn jump_inbound(
        &mut self,
        now: UtcTimestamp,
        seq: SeqNum,
        new_seq: SeqNum,
        out: &mut Vec<Action>,
    ) {
        if new_seq >= self.next_inbound {
            self.next_inbound = new_seq;
            self.finish_recovery();
        } else {
            let reject = SessionReject {
                ref_seq: seq,
                ref_tag: Some(Tag::NEW_SEQ_NO),
                reason: RejectReason::ValueIncorrect,
            };
            self.send_reject(now, reject, out);
        }
    }

    fn request_resend(&mut self, now: UtcTimestamp, seen: SeqNum, out: &mut Vec<Action>) {
        let Link::Active(active) = &mut self.link else {
            return;
        };
        let first_sight = active.recovering_through.is_none();
        active.recovering_through = Some(active.recovering_through.map_or(seen, |s| s.max(seen)));
        if first_sight {
            let resend = Admin::ResendRequest {
                begin: self.next_inbound,
                end: None,
            };
            self.transmit(now, Body::Admin(resend), out);
        }
    }

    fn finish_recovery(&mut self) {
        if let Link::Active(active) = &mut self.link
            && active
                .recovering_through
                .is_some_and(|through| self.next_inbound > through)
        {
            active.recovering_through = None;
        }
    }

    /// Reenvia com o seq original e `PossDupFlag=Y`. Sequências de mensagens
    /// administrativas viram um único `GapFill` que pula para a próxima de aplicação.
    fn resend(
        &mut self,
        now: UtcTimestamp,
        begin: SeqNum,
        end: Option<SeqNum>,
        out: &mut Vec<Action>,
    ) {
        let Some(last) = self.sent.keys().next_back().copied() else {
            return;
        };
        let end = end.map_or(last, |end| end.min(last));
        if begin > end {
            return;
        }
        let mut gap: Option<(SeqNum, UtcTimestamp)> = None;
        for (&seq, sent) in self.sent.range(begin..=end) {
            let Some(app) = &sent.app else {
                gap.get_or_insert((seq, sent.time));
                continue;
            };
            if let Some((start, time)) = gap.take() {
                out.push(Action::Send(self.gap_fill(now, start, time, seq)));
            }
            let body = Body::App(app.clone());
            out.push(Action::Send(self.message(now, seq, Some(sent.time), body)));
        }
        if let Some((start, time)) = gap {
            out.push(Action::Send(self.gap_fill(
                now,
                start,
                time,
                following(end),
            )));
        }
        self.touch_sent(now);
    }

    fn keep_alive(&mut self, now: UtcTimestamp, out: &mut Vec<Action>) {
        let Link::Active(active) = self.link else {
            return;
        };
        let interval = active.heartbeat_ms;
        match active.test_request_sent {
            Some(sent) if now.saturating_millis_since(sent) >= interval => {
                return self.terminate(now, "sem resposta ao TestRequest", out);
            }
            None if now.saturating_millis_since(active.last_received)
                >= interval + interval / 5 =>
            {
                self.test_requests += 1;
                let test_req_id = Text::new(format!("TEST-{}", self.test_requests))
                    .expect("TEST-n nunca é vazio nem contém SOH");
                self.transmit(now, Body::Admin(Admin::TestRequest { test_req_id }), out);
                if let Link::Active(active) = &mut self.link {
                    active.test_request_sent = Some(now);
                }
            }
            Some(_) | None => {}
        }
        if let Link::Active(active) = self.link
            && now.saturating_millis_since(active.last_sent) >= interval
        {
            self.transmit(
                now,
                Body::Admin(Admin::Heartbeat { test_req_id: None }),
                out,
            );
        }
    }

    fn flush_pending(&mut self, now: UtcTimestamp, out: &mut Vec<Action>) {
        while let Some(app) = self.pending.pop_front() {
            self.transmit(now, Body::App(app), out);
        }
    }

    fn transmit(&mut self, now: UtcTimestamp, body: Body, out: &mut Vec<Action>) {
        let seq = self.next_outbound;
        self.next_outbound = following(seq);
        let app = match &body {
            Body::App(app) => Some(app.clone()),
            Body::Admin(_) => None,
        };
        self.sent.insert(seq, Sent { time: now, app });
        out.push(Action::Send(self.message(now, seq, None, body)));
        self.touch_sent(now);
    }

    fn send_reject(&mut self, now: UtcTimestamp, reject: SessionReject, out: &mut Vec<Action>) {
        let admin = Admin::Reject {
            ref_seq: reject.ref_seq,
            ref_tag: reject.ref_tag,
            reason: Some(reject.reason),
            text: None,
        };
        self.transmit(now, Body::Admin(admin), out);
    }

    /// Encerramento por violação de protocolo: Logout com o motivo e queda da conexão,
    /// sem esperar resposta.
    fn terminate(&mut self, now: UtcTimestamp, reason: &str, out: &mut Vec<Action>) {
        let text = Text::new(reason).expect("motivos são literais não vazios e sem SOH");
        self.transmit(now, Body::Admin(Admin::Logout { text: Some(text) }), out);
        self.drop_link(out);
    }

    fn drop_link(&mut self, out: &mut Vec<Action>) {
        self.link = Link::Disconnected;
        out.push(Action::Disconnect);
    }

    fn touch_sent(&mut self, now: UtcTimestamp) {
        if let Link::Active(active) = &mut self.link {
            active.last_sent = now;
        }
    }

    fn is_from_counterparty(&self, header: &Header) -> bool {
        header.sender == self.config.target && header.target == self.config.sender
    }

    fn gap_fill(
        &self,
        now: UtcTimestamp,
        seq: SeqNum,
        orig: UtcTimestamp,
        new_seq: SeqNum,
    ) -> Message {
        let gap_fill = Admin::SequenceReset {
            gap_fill: true,
            new_seq,
        };
        self.message(now, seq, Some(orig), Body::Admin(gap_fill))
    }

    fn message(
        &self,
        now: UtcTimestamp,
        seq: SeqNum,
        orig_sending_time: Option<UtcTimestamp>,
        body: Body,
    ) -> Message {
        let header = Header {
            sender: self.config.sender.clone(),
            target: self.config.target.clone(),
            seq,
            sending_time: now,
            orig_sending_time,
        };
        Envelope { header, body }.to_message()
    }
}

/// `u64` de mensagens não se esgota: a um bilhão por segundo levaria 584 anos.
fn following(seq: SeqNum) -> SeqNum {
    seq.next().expect("seq de 64 bits não se esgota")
}

#[cfg(test)]
mod tests;
