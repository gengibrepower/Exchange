//! Camada de aplicação do FIX (seção 4.5): `NewOrderSingle` (`35=D`) e
//! `OrderCancelRequest` (`35=F`) viram intenções do núcleo, e os eventos voltam como
//! `ExecutionReport` (`35=8`), `OrderCancelReject` (`35=9`) ou `BusinessMessageReject`
//! (`35=j`). A sessão já garante que nenhuma mensagem é processada duas vezes; aqui o
//! `ClOrdID` garante o mesmo para a ordem, mesmo que o cliente a mande de novo com outro seq.
//!
//! Dois desvios assumidos do FIX 4.4, pelos mesmos motivos do protocolo JSON:
//! - Preço (`44`, `31`) é inteiro, em unidades-mínimas da quote por lot (seção 3.1), e o
//!   `AvgPx` (`6`) não é enviado, porque preço médio exige divisão.
//! - Só quem enviou a ordem que gerou o negócio recebe o relatório. Uma ordem parada no livro
//!   não é avisada quando outra conta a executa, como já acontece na borda JSON.

use std::collections::{HashMap, HashSet};
use std::fmt;

use exchange_core::domain::{
    AccountId, InstrumentId, Lots, OrderId, OrderKind, Price, Side, TimeInForce,
};
use exchange_core::engine::{Event, RejectReason};
use exchange_fix::{AppMessage, Field, Message, SeqNum, Tag, Text};

use crate::market::{instrument_id, symbol};
use crate::protocol::reject_message;
use crate::sequencer::Intent;

/// Só usada em constantes: o `panic!` acontece em tempo de compilação.
const fn tag(number: u32) -> Tag {
    match Tag::new(number) {
        Some(tag) => tag,
        None => panic!("tag FIX nunca é zero"),
    }
}

const CL_ORD_ID: Tag = tag(11);
const CUM_QTY: Tag = tag(14);
const EXEC_ID: Tag = tag(17);
const LAST_PX: Tag = tag(31);
const LAST_QTY: Tag = tag(32);
const ORDER_ID: Tag = tag(37);
const ORDER_QTY: Tag = tag(38);
const ORD_STATUS: Tag = tag(39);
const ORD_TYPE: Tag = tag(40);
const ORIG_CL_ORD_ID: Tag = tag(41);
const PRICE: Tag = tag(44);
const SIDE: Tag = tag(54);
const SYMBOL: Tag = tag(55);
const TIME_IN_FORCE: Tag = tag(59);
const CXL_REJ_REASON: Tag = tag(102);
const ORD_REJ_REASON: Tag = tag(103);
const EXEC_TYPE: Tag = tag(150);
const LEAVES_QTY: Tag = tag(151);
const REF_MSG_TYPE: Tag = tag(372);
const BUSINESS_REJECT_REASON: Tag = tag(380);
const CXL_REJ_RESPONSE_TO: Tag = tag(434);

/// O `OrderID` de quem ainda não tem um: recusado antes de chegar ao núcleo.
const NO_ORDER: &str = "NONE";

/// O que fazer com uma mensagem de aplicação.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Request {
    /// Recusada antes do núcleo; a resposta já está pronta.
    Reply(AppMessage),
    /// A borda manda o `intent` ao núcleo e devolve os eventos com o `pending` a
    /// [`FixOrders::outcome`].
    Submit { intent: Intent, pending: Pending },
}

/// O contexto de um pedido que foi ao núcleo, opaco para a borda.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pending(Kind);

#[derive(Clone, PartialEq, Eq, Debug)]
enum Kind {
    New(NewOrder),
    Cancel {
        cl_ord_id: Text,
        orig_cl_ord_id: Text,
        order: OrderId,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct NewOrder {
    cl_ord_id: Text,
    instrument: InstrumentId,
    side: Side,
    quantity: Lots,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Closed {
    Cancelled,
    Expired,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Tracked {
    order: NewOrder,
    filled: Lots,
    closed: Option<Closed>,
}

impl Tracked {
    fn status(&self) -> &'static str {
        match self.closed {
            Some(Closed::Cancelled) => "4",
            Some(Closed::Expired) => "C",
            None if self.filled == self.order.quantity => "2",
            None if self.filled == Lots::ZERO => "0",
            None => "1",
        }
    }

    fn leaves(&self) -> Lots {
        match self.closed {
            Some(_) => Lots::ZERO,
            None => self
                .order
                .quantity
                .checked_sub(self.filled)
                .unwrap_or(Lots::ZERO),
        }
    }

    fn is_done(&self) -> bool {
        self.leaves() == Lots::ZERO
    }
}

enum Execution {
    New,
    Trade {
        price: Price,
        lots: Lots,
    },
    /// `request` traz os `ClOrdID` do pedido de cancelamento; sem ele, foi o núcleo que
    /// cancelou a sobra de uma IOC.
    Cancelled {
        request: Option<(Text, Text)>,
    },
    Expired,
}

/// As ordens FIX de uma conta. Vive tanto quanto a sessão (através de quedas de conexão),
/// para que o `ClOrdID` de uma ordem antiga ainda seja reconhecido depois de reconectar.
#[derive(Clone, Debug)]
pub struct FixOrders {
    account: AccountId,
    orders: HashMap<OrderId, Tracked>,
    by_cl_ord_id: HashMap<Text, OrderId>,
    used_cl_ord_ids: HashSet<Text>,
    executions: u64,
}

impl FixOrders {
    pub fn new(account: AccountId) -> Self {
        Self {
            account,
            orders: HashMap::new(),
            by_cl_ord_id: HashMap::new(),
            used_cl_ord_ids: HashSet::new(),
            executions: 0,
        }
    }

    pub fn request(&mut self, seq: SeqNum, app: &AppMessage) -> Request {
        let message = app.message();
        let parsed = match message.msg_type() {
            b"D" => self.new_order(message),
            b"F" => self.cancel(message),
            _ => {
                return Request::Reply(business_reject(
                    seq,
                    message,
                    BusinessReason::UnsupportedMsgType,
                    "tipo de mensagem não suportado",
                ));
            }
        };
        parsed.unwrap_or_else(|problem| {
            let reason = if problem.missing {
                BusinessReason::RequiredFieldMissing
            } else {
                BusinessReason::Other
            };
            Request::Reply(business_reject(seq, message, reason, &problem.to_string()))
        })
    }

    pub fn outcome(&mut self, pending: Pending, events: &[Event]) -> Vec<AppMessage> {
        match pending.0 {
            Kind::New(order) => self.new_order_outcome(order, events),
            Kind::Cancel {
                cl_ord_id,
                orig_cl_ord_id,
                order,
            } => self.cancel_outcome(order, cl_ord_id, orig_cl_ord_id, events),
        }
    }

    fn new_order(&mut self, message: &Message) -> Result<Request, Problem> {
        let fields = Fields(message);
        let cl_ord_id = fields.required(CL_ORD_ID, text)?;
        let instrument = fields.required(SYMBOL, known_symbol)?;
        let side = fields.required(SIDE, side)?;
        let quantity = fields.required(ORDER_QTY, |raw| Lots::new(integer(raw)?).ok())?;
        let kind = match fields.required(ORD_TYPE, Some)? {
            b"1" => OrderKind::Market { protection: None },
            b"2" => OrderKind::Limit(fields.required(PRICE, price)?),
            _ => return Err(Problem::invalid(ORD_TYPE)),
        };
        let time_in_force = fields
            .optional(TIME_IN_FORCE, time_in_force)?
            .unwrap_or_default();
        let order = NewOrder {
            cl_ord_id,
            instrument,
            side,
            quantity,
        };
        if !self.used_cl_ord_ids.insert(order.cl_ord_id.clone()) {
            let text = "ClOrdID já usado por outra ordem";
            return Ok(Request::Reply(self.rejection(&order, None, "6", text)));
        }
        Ok(Request::Submit {
            intent: Intent::Order {
                account: self.account,
                instrument,
                side,
                kind,
                lots: quantity,
                time_in_force,
            },
            pending: Pending(Kind::New(order)),
        })
    }

    fn cancel(&mut self, message: &Message) -> Result<Request, Problem> {
        let fields = Fields(message);
        let cl_ord_id = fields.required(CL_ORD_ID, text)?;
        let orig_cl_ord_id = fields.required(ORIG_CL_ORD_ID, text)?;
        let reject = |reason, text, order| {
            Ok(Request::Reply(cancel_reject(
                &cl_ord_id,
                &orig_cl_ord_id,
                order,
                reason,
                text,
            )))
        };
        if !self.used_cl_ord_ids.insert(cl_ord_id.clone()) {
            return reject(CancelReason::DuplicateClOrdId, "ClOrdID já usado", None);
        }
        let Some((&order, tracked)) = self
            .by_cl_ord_id
            .get(&orig_cl_ord_id)
            .and_then(|order| self.orders.get_key_value(order))
        else {
            return reject(CancelReason::UnknownOrder, "OrigClOrdID desconhecido", None);
        };
        if tracked.is_done() {
            return reject(
                CancelReason::TooLate,
                "ordem já terminou",
                Some((order, tracked)),
            );
        }
        let instrument = tracked.order.instrument;
        Ok(Request::Submit {
            intent: Intent::Cancel {
                account: self.account,
                instrument,
                order,
            },
            pending: Pending(Kind::Cancel {
                cl_ord_id,
                orig_cl_ord_id,
                order,
            }),
        })
    }

    fn new_order_outcome(&mut self, order: NewOrder, events: &[Event]) -> Vec<AppMessage> {
        let Some(id) = events.iter().find_map(new_order_id) else {
            return Vec::new();
        };
        if let Some(reason) = events.iter().find_map(|event| match event {
            Event::Rejected { order, reason } if *order == id => Some(reason),
            _ => None,
        }) {
            let code = ord_rej_reason(reason);
            return vec![self.rejection(&order, Some(id), code, &reject_message(reason))];
        }
        self.by_cl_ord_id.insert(order.cl_ord_id.clone(), id);
        self.orders.insert(
            id,
            Tracked {
                order,
                filled: Lots::ZERO,
                closed: None,
            },
        );
        let mut reports = vec![self.execution(id, &Execution::New)];
        for event in events {
            match *event {
                Event::Fill { fill, .. } => {
                    for side in [fill.taker, fill.maker] {
                        if let Some(tracked) = self.orders.get_mut(&side) {
                            tracked.filled = tracked
                                .filled
                                .checked_add(fill.lots)
                                .expect("fills nunca passam da quantidade, que cabe em i64");
                            let trade = Execution::Trade {
                                price: fill.price,
                                lots: fill.lots,
                            };
                            reports.push(self.execution(side, &trade));
                        }
                    }
                }
                Event::Cancelled { order, .. } if self.close(order, Closed::Cancelled) => {
                    let cancelled = Execution::Cancelled { request: None };
                    reports.push(self.execution(order, &cancelled));
                }
                Event::Expired { order, .. } if self.close(order, Closed::Expired) => {
                    reports.push(self.execution(order, &Execution::Expired));
                }
                _ => {}
            }
        }
        reports
    }

    fn cancel_outcome(
        &mut self,
        id: OrderId,
        cl_ord_id: Text,
        orig_cl_ord_id: Text,
        events: &[Event],
    ) -> Vec<AppMessage> {
        for event in events {
            match event {
                Event::Cancelled { order, .. } if *order == id => {
                    self.close(id, Closed::Cancelled);
                    let cancelled = Execution::Cancelled {
                        request: Some((cl_ord_id, orig_cl_ord_id)),
                    };
                    return vec![self.execution(id, &cancelled)];
                }
                Event::Rejected { order, reason } if *order == id => {
                    let reason = match reason {
                        RejectReason::UnknownOrder => CancelReason::TooLate,
                        _ => CancelReason::Other,
                    };
                    let tracked = self.orders.get(&id).map(|tracked| (id, tracked));
                    let text = "o núcleo recusou o cancelamento";
                    return vec![cancel_reject(
                        &cl_ord_id,
                        &orig_cl_ord_id,
                        tracked,
                        reason,
                        text,
                    )];
                }
                _ => {}
            }
        }
        Vec::new()
    }

    fn close(&mut self, order: OrderId, closed: Closed) -> bool {
        match self.orders.get_mut(&order) {
            Some(tracked) => {
                tracked.closed = Some(closed);
                true
            }
            None => false,
        }
    }

    fn next_exec_id(&mut self) -> String {
        self.executions += 1;
        format!("E{}", self.executions)
    }

    fn execution(&mut self, id: OrderId, execution: &Execution) -> AppMessage {
        let exec_id = self.next_exec_id();
        let tracked = &self.orders[&id];
        let order = &tracked.order;
        let mut out = Out::default()
            .number(ORDER_ID, id.get())
            .text(CL_ORD_ID, &order.cl_ord_id)
            .str(EXEC_ID, &exec_id);
        out = match execution {
            Execution::New => out.str(EXEC_TYPE, "0"),
            Execution::Trade { price, lots } => out
                .str(EXEC_TYPE, "F")
                .number(LAST_PX, price.get())
                .number(LAST_QTY, lots.get()),
            Execution::Cancelled {
                request: Some((cl_ord_id, orig_cl_ord_id)),
            } => out
                .replace(CL_ORD_ID, cl_ord_id)
                .text(ORIG_CL_ORD_ID, orig_cl_ord_id)
                .str(EXEC_TYPE, "4"),
            Execution::Cancelled { request: None } => out.str(EXEC_TYPE, "4"),
            Execution::Expired => out.str(EXEC_TYPE, "C"),
        };
        out.str(ORD_STATUS, tracked.status())
            .order(order)
            .number(LEAVES_QTY, tracked.leaves().get())
            .number(CUM_QTY, tracked.filled.get())
            .build("8")
    }

    fn rejection(
        &mut self,
        order: &NewOrder,
        id: Option<OrderId>,
        code: &str,
        text: &str,
    ) -> AppMessage {
        let exec_id = self.next_exec_id();
        let out = match id {
            Some(id) => Out::default().number(ORDER_ID, id.get()),
            None => Out::default().str(ORDER_ID, NO_ORDER),
        };
        out.text(CL_ORD_ID, &order.cl_ord_id)
            .str(EXEC_ID, &exec_id)
            .str(EXEC_TYPE, "8")
            .str(ORD_STATUS, "8")
            .order(order)
            .number(LEAVES_QTY, 0)
            .number(CUM_QTY, 0)
            .str(ORD_REJ_REASON, code)
            .str(Tag::TEXT, text)
            .build("8")
    }
}

fn new_order_id(event: &Event) -> Option<OrderId> {
    match *event {
        Event::Rejected { order, .. }
        | Event::Rested { order, .. }
        | Event::Expired { order, .. }
        | Event::Cancelled { order, .. }
        | Event::Armed { order, .. } => Some(order),
        Event::Fill { fill, .. } => Some(fill.taker),
        Event::Deposited { .. } | Event::DepositRejected { .. } | Event::Triggered { .. } => None,
    }
}

fn ord_rej_reason(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::UnknownInstrument(_) => "1",
        RejectReason::OrderTooLarge { .. } => "3",
        RejectReason::DuplicateOrderId => "6",
        _ => "99",
    }
}

#[derive(Clone, Copy)]
enum CancelReason {
    TooLate,
    UnknownOrder,
    DuplicateClOrdId,
    Other,
}

impl CancelReason {
    fn code(self) -> &'static str {
        match self {
            Self::TooLate => "0",
            Self::UnknownOrder => "1",
            Self::DuplicateClOrdId => "6",
            Self::Other => "99",
        }
    }
}

fn cancel_reject(
    cl_ord_id: &Text,
    orig_cl_ord_id: &Text,
    order: Option<(OrderId, &Tracked)>,
    reason: CancelReason,
    text: &str,
) -> AppMessage {
    let out = match order {
        Some((id, _)) => Out::default().number(ORDER_ID, id.get()),
        None => Out::default().str(ORDER_ID, NO_ORDER),
    };
    out.text(CL_ORD_ID, cl_ord_id)
        .text(ORIG_CL_ORD_ID, orig_cl_ord_id)
        .str(
            ORD_STATUS,
            order.map_or("8", |(_, tracked)| tracked.status()),
        )
        .str(CXL_REJ_RESPONSE_TO, "1")
        .str(CXL_REJ_REASON, reason.code())
        .str(Tag::TEXT, text)
        .build("9")
}

#[derive(Clone, Copy)]
enum BusinessReason {
    Other,
    UnsupportedMsgType,
    RequiredFieldMissing,
}

fn business_reject(
    seq: SeqNum,
    message: &Message,
    reason: BusinessReason,
    text: &str,
) -> AppMessage {
    let code = match reason {
        BusinessReason::Other => "0",
        BusinessReason::UnsupportedMsgType => "3",
        BusinessReason::RequiredFieldMissing => "5",
    };
    Out::default()
        .number(Tag::REF_SEQ_NUM, seq.get())
        .bytes(REF_MSG_TYPE, message.msg_type())
        .str(BUSINESS_REJECT_REASON, code)
        .str(Tag::TEXT, text)
        .build("j")
}

/// Um campo obrigatório ausente ou um valor que não se entende.
struct Problem {
    tag: Tag,
    missing: bool,
}

impl Problem {
    fn invalid(tag: Tag) -> Self {
        Self {
            tag,
            missing: false,
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = if self.missing { "ausente" } else { "inválida" };
        write!(f, "tag {} {what}", self.tag)
    }
}

struct Fields<'a>(&'a Message);

impl<'a> Fields<'a> {
    fn optional<T>(
        &self,
        tag: Tag,
        parse: impl FnOnce(&'a [u8]) -> Option<T>,
    ) -> Result<Option<T>, Problem> {
        self.0
            .body()
            .iter()
            .find(|field| field.tag() == tag)
            .map(|field| parse(field.value()).ok_or(Problem::invalid(tag)))
            .transpose()
    }

    fn required<T>(
        &self,
        tag: Tag,
        parse: impl FnOnce(&'a [u8]) -> Option<T>,
    ) -> Result<T, Problem> {
        self.optional(tag, parse)?
            .ok_or(Problem { tag, missing: true })
    }
}

fn text(raw: &[u8]) -> Option<Text> {
    Text::new(raw).ok()
}

fn known_symbol(raw: &[u8]) -> Option<InstrumentId> {
    instrument_id(std::str::from_utf8(raw).ok()?)
}

fn side(raw: &[u8]) -> Option<Side> {
    match raw {
        b"1" => Some(Side::Bid),
        b"2" => Some(Side::Ask),
        _ => None,
    }
}

fn time_in_force(raw: &[u8]) -> Option<TimeInForce> {
    match raw {
        b"1" => Some(TimeInForce::Gtc),
        b"3" => Some(TimeInForce::Ioc),
        b"4" => Some(TimeInForce::Fok),
        _ => None,
    }
}

fn price(raw: &[u8]) -> Option<Price> {
    Price::new(integer(raw)?).ok()
}

/// Só dígitos: `str::parse` aceitaria `+5`, e o FIX não tem sinal em quantidade nem preço.
fn integer(raw: &[u8]) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    raw.iter().try_fold(0i64, |number, &digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        number.checked_mul(10)?.checked_add(i64::from(digit - b'0'))
    })
}

/// Monta as mensagens de saída. Todos os valores são números, códigos fixos, textos já
/// validados ou mensagens de erro literais: nunca vazios nem com SOH, e nenhuma tag é do
/// envelope ou do header. Por isso os `expect` daqui não são alcançáveis.
#[derive(Default)]
struct Out {
    fields: Vec<Field>,
}

impl Out {
    fn bytes(mut self, tag: Tag, value: &[u8]) -> Self {
        let field = Field::new(tag, value).expect("valores de saída nunca são vazios nem têm SOH");
        self.fields.push(field);
        self
    }

    fn str(self, tag: Tag, value: &str) -> Self {
        self.bytes(tag, value.as_bytes())
    }

    fn text(self, tag: Tag, value: &Text) -> Self {
        self.bytes(tag, value.as_bytes())
    }

    fn number(self, tag: Tag, value: impl fmt::Display) -> Self {
        self.str(tag, &value.to_string())
    }

    fn replace(mut self, tag: Tag, value: &Text) -> Self {
        self.fields.retain(|field| field.tag() != tag);
        self.text(tag, value)
    }

    fn order(self, order: &NewOrder) -> Self {
        let side = match order.side {
            Side::Bid => "1",
            Side::Ask => "2",
        };
        self.str(SYMBOL, symbol(order.instrument))
            .str(SIDE, side)
            .number(ORDER_QTY, order.quantity.get())
    }

    fn build(self, msg_type: &str) -> AppMessage {
        let message = Message::new(msg_type, self.fields).expect("nenhuma tag do envelope");
        AppMessage::new(message).expect("tipos de aplicação, sem tags do header")
    }
}

#[cfg(test)]
mod tests;
