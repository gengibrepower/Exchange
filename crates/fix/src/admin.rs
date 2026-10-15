use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use crate::{Field, InvalidValue, Message, Tag, UtcTimestamp, parse_number, validate_value};

impl Tag {
    pub const BEGIN_SEQ_NO: Self = Self::known(7);
    pub const END_SEQ_NO: Self = Self::known(16);
    pub const MSG_SEQ_NUM: Self = Self::known(34);
    pub const NEW_SEQ_NO: Self = Self::known(36);
    pub const POSS_DUP_FLAG: Self = Self::known(43);
    pub const REF_SEQ_NUM: Self = Self::known(45);
    pub const SENDER_COMP_ID: Self = Self::known(49);
    pub const SENDING_TIME: Self = Self::known(52);
    pub const TARGET_COMP_ID: Self = Self::known(56);
    pub const TEXT: Self = Self::known(58);
    pub const ENCRYPT_METHOD: Self = Self::known(98);
    pub const HEART_BT_INT: Self = Self::known(108);
    pub const TEST_REQ_ID: Self = Self::known(112);
    pub const ORIG_SENDING_TIME: Self = Self::known(122);
    pub const GAP_FILL_FLAG: Self = Self::known(123);
    pub const REF_TAG_ID: Self = Self::known(371);
    pub const SESSION_REJECT_REASON: Self = Self::known(373);
}

const HEADER_TAGS: [Tag; 6] = [
    Tag::SENDER_COMP_ID,
    Tag::TARGET_COMP_ID,
    Tag::MSG_SEQ_NUM,
    Tag::SENDING_TIME,
    Tag::POSS_DUP_FLAG,
    Tag::ORIG_SENDING_TIME,
];

const ADMIN_MSG_TYPES: [&[u8]; 7] = [b"A", b"0", b"1", b"2", b"3", b"4", b"5"];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SeqNum(NonZeroU64);

impl SeqNum {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub const fn new(number: u64) -> Option<Self> {
        match NonZeroU64::new(number) {
            Some(number) => Some(Self(number)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for SeqNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Valor livre de um campo: `CompID`, `TestReqID`, `ClOrdID`, texto de Logout. Mesma regra
/// de qualquer valor FIX, não vazio e sem SOH. `Hash` porque `CompID` e `ClOrdID` são
/// chaves de mapa na borda.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Text(Vec<u8>);

impl Text {
    /// # Errors
    /// [`InvalidValue`] se o texto é vazio ou contém SOH.
    pub fn new(text: impl Into<Vec<u8>>) -> Result<Self, InvalidValue> {
        let text = text.into();
        validate_value(&text)?;
        Ok(Self(text))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Header {
    pub sender: Text,
    pub target: Text,
    pub seq: SeqNum,
    pub sending_time: UtcTimestamp,
    /// Presente se e só se `PossDupFlag=Y`: é o `OrigSendingTime` da primeira transmissão.
    /// Juntar os dois num `Option` torna impossível um reenvio sem a hora original.
    pub orig_sending_time: Option<UtcTimestamp>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Admin {
    Logon {
        heartbeat_secs: NonZeroU32,
    },
    Heartbeat {
        test_req_id: Option<Text>,
    },
    TestRequest {
        test_req_id: Text,
    },
    /// `end: None` é o `EndSeqNo=0` do fio: "até a última que você enviou".
    ResendRequest {
        begin: SeqNum,
        end: Option<SeqNum>,
    },
    /// Um motivo fora dos que modelamos chega como `None`: o Reject é informativo, e
    /// rejeitar o Reject do outro lado abriria um pingue-pongue sem fim.
    Reject {
        ref_seq: SeqNum,
        ref_tag: Option<Tag>,
        reason: Option<RejectReason>,
        text: Option<Text>,
    },
    SequenceReset {
        gap_fill: bool,
        new_seq: SeqNum,
    },
    Logout {
        text: Option<Text>,
    },
}

/// Os motivos de `SessionRejectReason` (373) que esta sessão emite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    RequiredTagMissing,
    ValueIncorrect,
    IncorrectDataFormat,
    CompIdProblem,
    SendingTimeAccuracy,
    InvalidMsgType,
}

impl RejectReason {
    pub fn code(self) -> u64 {
        match self {
            Self::RequiredTagMissing => 1,
            Self::ValueIncorrect => 5,
            Self::IncorrectDataFormat => 6,
            Self::CompIdProblem => 9,
            Self::SendingTimeAccuracy => 10,
            Self::InvalidMsgType => 11,
        }
    }

    fn from_code(code: u64) -> Option<Self> {
        [
            Self::RequiredTagMissing,
            Self::ValueIncorrect,
            Self::IncorrectDataFormat,
            Self::CompIdProblem,
            Self::SendingTimeAccuracy,
            Self::InvalidMsgType,
        ]
        .into_iter()
        .find(|reason| reason.code() == code)
    }
}

/// Mensagem de aplicação: qualquer `MsgType` fora dos administrativos, sem as tags do
/// header, que são da sessão. A camada de aplicação valida o resto.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AppMessage(Message);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotApp {
    AdminMsgType,
    HeaderTag(Tag),
}

impl fmt::Display for NotApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdminMsgType => f.write_str("MsgType é de sessão, não de aplicação"),
            Self::HeaderTag(tag) => write!(f, "tag {tag} é do header da sessão"),
        }
    }
}

impl std::error::Error for NotApp {}

impl AppMessage {
    /// # Errors
    /// [`NotApp`] se o `MsgType` é administrativo ou se o corpo traz uma tag do header.
    pub fn new(message: Message) -> Result<Self, NotApp> {
        if ADMIN_MSG_TYPES.contains(&message.msg_type()) {
            return Err(NotApp::AdminMsgType);
        }
        if let Some(field) = message
            .body()
            .iter()
            .find(|f| HEADER_TAGS.contains(&f.tag()))
        {
            return Err(NotApp::HeaderTag(field.tag()));
        }
        Ok(Self(message))
    }

    pub fn message(&self) -> &Message {
        &self.0
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Body {
    Admin(Admin),
    App(AppMessage),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Envelope {
    pub header: Header,
    pub body: Body,
}

/// Erro que a sessão responde com um Reject (`35=3`) apontando a mensagem e a tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SessionReject {
    pub ref_seq: SeqNum,
    pub ref_tag: Option<Tag>,
    pub reason: RejectReason,
}

impl fmt::Display for SessionReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mensagem {} recusada: {:?}", self.ref_seq, self.reason)?;
        if let Some(tag) = self.ref_tag {
            write!(f, " na tag {tag}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SessionReject {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    /// Sem um `MsgSeqNum` legível não há como referenciar a mensagem num Reject: o FIX
    /// manda encerrar a sessão com Logout.
    Unsequenced,
    Reject(SessionReject),
}

impl From<SessionReject> for ParseError {
    fn from(reject: SessionReject) -> Self {
        Self::Reject(reject)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsequenced => f.write_str("mensagem sem MsgSeqNum válido"),
            Self::Reject(reject) => reject.fmt(f),
        }
    }
}

impl std::error::Error for ParseError {}

impl Envelope {
    /// # Errors
    /// [`ParseError::Unsequenced`] se falta o `MsgSeqNum`; [`ParseError::Reject`] se o
    /// header ou uma mensagem administrativa vem incompleta ou malformada.
    pub fn parse(message: Message) -> Result<Self, ParseError> {
        let seq = message
            .body()
            .iter()
            .find(|field| field.tag() == Tag::MSG_SEQ_NUM)
            .and_then(|field| seq_num(field.value()))
            .ok_or(ParseError::Unsequenced)?;
        let reader = Reader {
            message: &message,
            seq,
        };
        let header = Header::parse(&reader)?;
        let body = match Admin::parse(&reader)? {
            Some(admin) => Body::Admin(admin),
            None => Body::App(AppMessage(Message {
                msg_type: message.msg_type,
                body: message
                    .body
                    .into_iter()
                    .filter(|field| !HEADER_TAGS.contains(&field.tag()))
                    .collect(),
            })),
        };
        Ok(Self { header, body })
    }

    pub fn to_message(&self) -> Message {
        let mut fields = self.header.fields();
        let msg_type = match &self.body {
            Body::Admin(admin) => {
                fields.extend(admin.fields());
                admin.msg_type().to_vec()
            }
            Body::App(app) => {
                fields.extend(app.0.body().iter().cloned());
                app.0.msg_type().to_vec()
            }
        };
        Message {
            msg_type,
            body: fields,
        }
    }
}

impl Header {
    fn parse(reader: &Reader<'_>) -> Result<Self, SessionReject> {
        let sender = reader.required(Tag::SENDER_COMP_ID, text)?;
        let target = reader.required(Tag::TARGET_COMP_ID, text)?;
        let sending_time = reader.required(Tag::SENDING_TIME, UtcTimestamp::parse)?;
        let orig_sending_time = match reader.optional(Tag::POSS_DUP_FLAG, flag)? {
            Some(true) => Some(reader.required(Tag::ORIG_SENDING_TIME, UtcTimestamp::parse)?),
            Some(false) | None => None,
        };
        Ok(Self {
            sender,
            target,
            seq: reader.seq,
            sending_time,
            orig_sending_time,
        })
    }

    fn fields(&self) -> Vec<Field> {
        let mut fields = vec![
            text_field(Tag::SENDER_COMP_ID, &self.sender),
            text_field(Tag::TARGET_COMP_ID, &self.target),
            shown_field(Tag::MSG_SEQ_NUM, self.seq),
            shown_field(Tag::SENDING_TIME, self.sending_time),
        ];
        if let Some(orig) = self.orig_sending_time {
            fields.push(flag_field(Tag::POSS_DUP_FLAG, true));
            fields.push(shown_field(Tag::ORIG_SENDING_TIME, orig));
        }
        fields
    }
}

impl Admin {
    fn parse(reader: &Reader<'_>) -> Result<Option<Self>, SessionReject> {
        let admin = match reader.message.msg_type() {
            b"A" => logon(reader)?,
            b"0" => Self::Heartbeat {
                test_req_id: reader.optional(Tag::TEST_REQ_ID, text)?,
            },
            b"1" => Self::TestRequest {
                test_req_id: reader.required(Tag::TEST_REQ_ID, text)?,
            },
            b"2" => resend_request(reader)?,
            b"3" => reject(reader)?,
            b"4" => sequence_reset(reader)?,
            b"5" => Self::Logout {
                text: reader.optional(Tag::TEXT, text)?,
            },
            _ => return Ok(None),
        };
        Ok(Some(admin))
    }

    fn msg_type(&self) -> &'static [u8] {
        match self {
            Self::Logon { .. } => b"A",
            Self::Heartbeat { .. } => b"0",
            Self::TestRequest { .. } => b"1",
            Self::ResendRequest { .. } => b"2",
            Self::Reject { .. } => b"3",
            Self::SequenceReset { .. } => b"4",
            Self::Logout { .. } => b"5",
        }
    }

    fn fields(&self) -> Vec<Field> {
        match self {
            Self::Logon { heartbeat_secs } => vec![
                shown_field(Tag::ENCRYPT_METHOD, 0),
                shown_field(Tag::HEART_BT_INT, heartbeat_secs),
            ],
            Self::Heartbeat { test_req_id } => test_req_id
                .iter()
                .map(|id| text_field(Tag::TEST_REQ_ID, id))
                .collect(),
            Self::TestRequest { test_req_id } => vec![text_field(Tag::TEST_REQ_ID, test_req_id)],
            Self::ResendRequest { begin, end } => vec![
                shown_field(Tag::BEGIN_SEQ_NO, begin),
                shown_field(Tag::END_SEQ_NO, end.map_or(0, SeqNum::get)),
            ],
            Self::Reject {
                ref_seq,
                ref_tag,
                reason,
                text,
            } => {
                let mut fields = vec![shown_field(Tag::REF_SEQ_NUM, ref_seq)];
                fields.extend(ref_tag.map(|tag| shown_field(Tag::REF_TAG_ID, tag)));
                fields.extend(
                    reason.map(|reason| shown_field(Tag::SESSION_REJECT_REASON, reason.code())),
                );
                fields.extend(text.iter().map(|text| text_field(Tag::TEXT, text)));
                fields
            }
            Self::SequenceReset { gap_fill, new_seq } => vec![
                flag_field(Tag::GAP_FILL_FLAG, *gap_fill),
                shown_field(Tag::NEW_SEQ_NO, new_seq),
            ],
            Self::Logout { text } => text
                .iter()
                .map(|text| text_field(Tag::TEXT, text))
                .collect(),
        }
    }
}

fn logon(reader: &Reader<'_>) -> Result<Admin, SessionReject> {
    if reader.required(Tag::ENCRYPT_METHOD, parse_number)? != 0 {
        return Err(reader.reject(RejectReason::ValueIncorrect, Tag::ENCRYPT_METHOD));
    }
    let heartbeat_secs = reader.required(Tag::HEART_BT_INT, parse_number)?;
    let heartbeat_secs = u32::try_from(heartbeat_secs)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| reader.reject(RejectReason::ValueIncorrect, Tag::HEART_BT_INT))?;
    Ok(Admin::Logon { heartbeat_secs })
}

fn reject(reader: &Reader<'_>) -> Result<Admin, SessionReject> {
    Ok(Admin::Reject {
        ref_seq: reader.required(Tag::REF_SEQ_NUM, seq_num)?,
        ref_tag: reader.optional(Tag::REF_TAG_ID, tag)?,
        reason: reader
            .optional(Tag::SESSION_REJECT_REASON, parse_number)?
            .and_then(RejectReason::from_code),
        text: reader.optional(Tag::TEXT, text)?,
    })
}

fn resend_request(reader: &Reader<'_>) -> Result<Admin, SessionReject> {
    let begin = reader.required(Tag::BEGIN_SEQ_NO, seq_num)?;
    let end = SeqNum::new(reader.required(Tag::END_SEQ_NO, parse_number)?);
    if end.is_some_and(|end| end < begin) {
        return Err(reader.reject(RejectReason::ValueIncorrect, Tag::END_SEQ_NO));
    }
    Ok(Admin::ResendRequest { begin, end })
}

fn sequence_reset(reader: &Reader<'_>) -> Result<Admin, SessionReject> {
    Ok(Admin::SequenceReset {
        gap_fill: reader.optional(Tag::GAP_FILL_FLAG, flag)?.unwrap_or(false),
        new_seq: reader.required(Tag::NEW_SEQ_NO, seq_num)?,
    })
}

/// Lê campos de uma mensagem já com o `MsgSeqNum` conhecido, para que todo erro saia
/// pronto para virar Reject apontando a mensagem e a tag.
struct Reader<'a> {
    message: &'a Message,
    seq: SeqNum,
}

impl<'a> Reader<'a> {
    fn raw(&self, tag: Tag) -> Option<&'a [u8]> {
        self.message
            .body()
            .iter()
            .find(|field| field.tag() == tag)
            .map(Field::value)
    }

    fn optional<T>(
        &self,
        tag: Tag,
        parse: impl FnOnce(&[u8]) -> Option<T>,
    ) -> Result<Option<T>, SessionReject> {
        self.raw(tag)
            .map(|raw| {
                parse(raw).ok_or_else(|| self.reject(RejectReason::IncorrectDataFormat, tag))
            })
            .transpose()
    }

    fn required<T>(
        &self,
        tag: Tag,
        parse: impl FnOnce(&[u8]) -> Option<T>,
    ) -> Result<T, SessionReject> {
        self.optional(tag, parse)?
            .ok_or_else(|| self.reject(RejectReason::RequiredTagMissing, tag))
    }

    fn reject(&self, reason: RejectReason, tag: Tag) -> SessionReject {
        SessionReject {
            ref_seq: self.seq,
            ref_tag: Some(tag),
            reason,
        }
    }
}

fn seq_num(raw: &[u8]) -> Option<SeqNum> {
    parse_number(raw).and_then(SeqNum::new)
}

fn tag(raw: &[u8]) -> Option<Tag> {
    parse_number(raw)
        .and_then(|number| u32::try_from(number).ok())
        .and_then(Tag::new)
}

fn text(raw: &[u8]) -> Option<Text> {
    Text::new(raw).ok()
}

fn flag(raw: &[u8]) -> Option<bool> {
    match raw {
        b"Y" => Some(true),
        b"N" => Some(false),
        _ => None,
    }
}

fn text_field(tag: Tag, text: &Text) -> Field {
    Field {
        tag,
        value: text.0.clone(),
    }
}

/// Para números e instantes: o `Display` deles nunca é vazio nem tem SOH, então o campo
/// sai válido sem passar pela checagem de [`Field::new`].
fn shown_field(tag: Tag, value: impl fmt::Display) -> Field {
    Field {
        tag,
        value: value.to_string().into_bytes(),
    }
}

fn flag_field(tag: Tag, value: bool) -> Field {
    Field {
        tag,
        value: if value { b"Y" } else { b"N" }.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::{Frame, decode, encode, parse_field};

    const HEADER: &str = "49=CLIENTE|56=EXCHANGE|34=7|52=20261015-12:00:00.000";

    fn message(msg_type: &str, fields: &str) -> Message {
        let body = fields
            .split('|')
            .filter(|raw| !raw.is_empty())
            .map(|raw| parse_field(raw.as_bytes()).expect("campo de teste bem formado"))
            .collect();
        Message::new(msg_type, body).expect("mensagem de teste válida")
    }

    fn parsed(msg_type: &str, fields: &str) -> Envelope {
        Envelope::parse(message(msg_type, fields)).expect("mensagem de teste aceita")
    }

    fn admin(msg_type: &str, fields: &str) -> Admin {
        match parsed(msg_type, &format!("{HEADER}|{fields}")).body {
            Body::Admin(admin) => admin,
            Body::App(app) => panic!("esperava administrativa, veio {app:?}"),
        }
    }

    fn rejected(msg_type: &str, fields: &str) -> SessionReject {
        match Envelope::parse(message(msg_type, fields)) {
            Err(ParseError::Reject(reject)) => reject,
            other => panic!("esperava Reject, veio {other:?}"),
        }
    }

    fn rejects(reason: RejectReason, tag: Tag) -> SessionReject {
        SessionReject {
            ref_seq: SeqNum::new(7).expect("seq de teste não é zero"),
            ref_tag: Some(tag),
            reason,
        }
    }

    fn seq(number: u64) -> SeqNum {
        SeqNum::new(number).expect("seq de teste não é zero")
    }

    #[test]
    fn seq_num_starts_at_one_and_never_wraps() {
        assert_eq!(SeqNum::new(0), None);
        assert_eq!(SeqNum::FIRST.next(), Some(seq(2)));
        assert_eq!(seq(u64::MAX).next(), None);
    }

    #[test]
    fn reads_the_header() {
        let header = parsed("0", HEADER).header;
        assert_eq!(header.sender.as_bytes(), b"CLIENTE");
        assert_eq!(header.target.as_bytes(), b"EXCHANGE");
        assert_eq!(header.seq, seq(7));
        assert_eq!(header.sending_time.to_string(), "20261015-12:00:00.000");
        assert_eq!(header.orig_sending_time, None);
    }

    #[test]
    fn missing_or_unreadable_seq_is_unsequenced() {
        for fields in [
            "49=A|56=B|52=20261015-12:00:00.000",
            "49=A|56=B|34=0|52=20261015-12:00:00.000",
            "49=A|56=B|34=x|52=20261015-12:00:00.000",
        ] {
            assert_eq!(
                Envelope::parse(message("0", fields)),
                Err(ParseError::Unsequenced),
                "{fields}"
            );
        }
    }

    #[test]
    fn missing_header_tags_are_rejected_by_tag() {
        assert_eq!(
            rejected("0", "56=B|34=7|52=20261015-12:00:00.000"),
            rejects(RejectReason::RequiredTagMissing, Tag::SENDER_COMP_ID)
        );
        assert_eq!(
            rejected("0", "49=A|34=7|52=20261015-12:00:00.000"),
            rejects(RejectReason::RequiredTagMissing, Tag::TARGET_COMP_ID)
        );
        assert_eq!(
            rejected("0", "49=A|56=B|34=7"),
            rejects(RejectReason::RequiredTagMissing, Tag::SENDING_TIME)
        );
    }

    #[test]
    fn malformed_sending_time_is_incorrect_format() {
        assert_eq!(
            rejected("0", "49=A|56=B|34=7|52=20260230-12:00:00.000"),
            rejects(RejectReason::IncorrectDataFormat, Tag::SENDING_TIME)
        );
    }

    #[test]
    fn possible_duplicate_requires_orig_sending_time() {
        let duplicate = parsed("0", &format!("{HEADER}|43=Y|122=20261015-11:00:00.000")).header;
        assert_eq!(
            duplicate.orig_sending_time.map(|t| t.to_string()),
            Some("20261015-11:00:00.000".to_string())
        );
        assert_eq!(
            rejected("0", &format!("{HEADER}|43=Y")),
            rejects(RejectReason::RequiredTagMissing, Tag::ORIG_SENDING_TIME)
        );
        assert_eq!(
            rejected("0", &format!("{HEADER}|43=S")),
            rejects(RejectReason::IncorrectDataFormat, Tag::POSS_DUP_FLAG)
        );
    }

    #[test]
    fn orig_sending_time_without_possible_duplicate_is_ignored() {
        let header = parsed("0", &format!("{HEADER}|43=N|122=20261015-11:00:00.000")).header;
        assert_eq!(header.orig_sending_time, None);
    }

    #[test]
    fn logon_requires_no_encryption_and_positive_heartbeat() {
        assert_eq!(
            admin("A", "98=0|108=30"),
            Admin::Logon {
                heartbeat_secs: NonZeroU32::new(30).expect("não é zero")
            }
        );
        assert_eq!(
            rejected("A", &format!("{HEADER}|98=1|108=30")),
            rejects(RejectReason::ValueIncorrect, Tag::ENCRYPT_METHOD)
        );
        assert_eq!(
            rejected("A", &format!("{HEADER}|98=0|108=0")),
            rejects(RejectReason::ValueIncorrect, Tag::HEART_BT_INT)
        );
        assert_eq!(
            rejected("A", &format!("{HEADER}|98=0")),
            rejects(RejectReason::RequiredTagMissing, Tag::HEART_BT_INT)
        );
    }

    #[test]
    fn resend_request_end_zero_means_up_to_the_last() {
        assert_eq!(
            admin("2", "7=5|16=0"),
            Admin::ResendRequest {
                begin: seq(5),
                end: None
            }
        );
        assert_eq!(
            admin("2", "7=5|16=9"),
            Admin::ResendRequest {
                begin: seq(5),
                end: Some(seq(9))
            }
        );
    }

    #[test]
    fn resend_request_rejects_bad_range() {
        assert_eq!(
            rejected("2", &format!("{HEADER}|7=9|16=5")),
            rejects(RejectReason::ValueIncorrect, Tag::END_SEQ_NO)
        );
        assert_eq!(
            rejected("2", &format!("{HEADER}|7=0|16=5")),
            rejects(RejectReason::IncorrectDataFormat, Tag::BEGIN_SEQ_NO)
        );
        assert_eq!(
            rejected("2", &format!("{HEADER}|16=5")),
            rejects(RejectReason::RequiredTagMissing, Tag::BEGIN_SEQ_NO)
        );
        assert_eq!(
            rejected("2", &format!("{HEADER}|7=5")),
            rejects(RejectReason::RequiredTagMissing, Tag::END_SEQ_NO)
        );
    }

    #[test]
    fn sequence_reset_gap_fill_defaults_to_no() {
        assert_eq!(
            admin("4", "36=12"),
            Admin::SequenceReset {
                gap_fill: false,
                new_seq: seq(12)
            }
        );
        assert_eq!(
            admin("4", "123=Y|36=12"),
            Admin::SequenceReset {
                gap_fill: true,
                new_seq: seq(12)
            }
        );
        assert_eq!(
            rejected("4", &format!("{HEADER}|123=Y")),
            rejects(RejectReason::RequiredTagMissing, Tag::NEW_SEQ_NO)
        );
        assert_eq!(
            rejected("4", &format!("{HEADER}|123=talvez|36=12")),
            rejects(RejectReason::IncorrectDataFormat, Tag::GAP_FILL_FLAG)
        );
    }

    #[test]
    fn reject_with_unknown_reason_still_parses() {
        assert_eq!(
            admin("3", "45=3|373=99"),
            Admin::Reject {
                ref_seq: seq(3),
                ref_tag: None,
                reason: None,
                text: None
            }
        );
    }

    #[test]
    fn app_message_loses_header_tags_and_keeps_its_fields() {
        let envelope = parsed("D", &format!("{HEADER}|11=ord-1|54=1"));
        assert_eq!(
            envelope.body,
            Body::App(AppMessage(message("D", "11=ord-1|54=1")))
        );
    }

    #[test]
    fn app_message_refuses_admin_type_and_header_tags() {
        assert_eq!(AppMessage::new(message("A", "")), Err(NotApp::AdminMsgType));
        assert_eq!(
            AppMessage::new(message("D", "11=x|34=2")),
            Err(NotApp::HeaderTag(Tag::MSG_SEQ_NUM))
        );
    }

    fn any_text() -> impl Strategy<Value = Text> {
        prop::collection::vec(
            any::<u8>().prop_filter("SOH delimita campos", |&b| b != 0x01),
            1..8,
        )
        .prop_map(|bytes| Text::new(bytes).expect("sem SOH e não vazio"))
    }

    fn any_seq() -> impl Strategy<Value = SeqNum> {
        (1..=u64::MAX).prop_map(seq)
    }

    fn any_time() -> impl Strategy<Value = UtcTimestamp> {
        (0..=UtcTimestamp::MAX.as_unix_millis())
            .prop_map(|millis| UtcTimestamp::from_unix_millis(millis).expect("dentro do limite"))
    }

    fn any_header() -> impl Strategy<Value = Header> {
        (
            any_text(),
            any_text(),
            any_seq(),
            any_time(),
            prop::option::of(any_time()),
        )
            .prop_map(
                |(sender, target, seq, sending_time, orig_sending_time)| Header {
                    sender,
                    target,
                    seq,
                    sending_time,
                    orig_sending_time,
                },
            )
    }

    fn any_reason() -> impl Strategy<Value = RejectReason> {
        prop::sample::select(vec![
            RejectReason::RequiredTagMissing,
            RejectReason::ValueIncorrect,
            RejectReason::IncorrectDataFormat,
            RejectReason::CompIdProblem,
            RejectReason::SendingTimeAccuracy,
            RejectReason::InvalidMsgType,
        ])
    }

    fn any_tag() -> impl Strategy<Value = Tag> {
        (1..=u32::MAX).prop_filter_map("tags do envelope ficam fora", |number| {
            Tag::new(number).filter(|tag| !tag.is_reserved() && !HEADER_TAGS.contains(tag))
        })
    }

    fn any_admin() -> impl Strategy<Value = Admin> {
        prop_oneof![
            (1..=u32::MAX).prop_map(|secs| Admin::Logon {
                heartbeat_secs: NonZeroU32::new(secs).expect("não é zero")
            }),
            prop::option::of(any_text()).prop_map(|test_req_id| Admin::Heartbeat { test_req_id }),
            any_text().prop_map(|test_req_id| Admin::TestRequest { test_req_id }),
            (any_seq(), any::<Option<u64>>()).prop_map(|(begin, extra)| Admin::ResendRequest {
                begin,
                end: extra
                    .and_then(|extra| begin.get().checked_add(extra))
                    .and_then(SeqNum::new),
            }),
            (
                any_seq(),
                prop::option::of(any_tag()),
                prop::option::of(any_reason()),
                prop::option::of(any_text())
            )
                .prop_map(|(ref_seq, ref_tag, reason, text)| Admin::Reject {
                    ref_seq,
                    ref_tag,
                    reason,
                    text
                }),
            (any::<bool>(), any_seq())
                .prop_map(|(gap_fill, new_seq)| Admin::SequenceReset { gap_fill, new_seq }),
            prop::option::of(any_text()).prop_map(|text| Admin::Logout { text }),
        ]
    }

    fn any_app() -> impl Strategy<Value = AppMessage> {
        let field = (any_tag(), any_text())
            .prop_map(|(tag, text)| Field::new(tag, text.0).expect("texto já é valor válido"));
        (
            prop::sample::select(vec!["D", "8", "F", "AE", "j"]),
            prop::collection::vec(field, 0..6),
        )
            .prop_map(|(msg_type, body)| {
                let message = Message::new(msg_type, body).expect("sem tags do envelope");
                AppMessage::new(message).expect("sem tipo de sessão nem tag do header")
            })
    }

    fn any_envelope() -> impl Strategy<Value = Envelope> {
        let body = prop_oneof![
            any_admin().prop_map(Body::Admin),
            any_app().prop_map(Body::App)
        ];
        (any_header(), body).prop_map(|(header, body)| Envelope { header, body })
    }

    proptest! {
        #[test]
        fn parse_inverts_to_message(envelope in any_envelope()) {
            prop_assert_eq!(Envelope::parse(envelope.to_message()), Ok(envelope));
        }

        #[test]
        fn survives_the_wire(envelope in any_envelope()) {
            let mut bytes = Vec::new();
            encode(&envelope.to_message(), &mut bytes).expect("estratégia fica abaixo do teto");
            let Frame::Complete { message, .. } = decode(&bytes) else {
                return Err(TestCaseError::fail("quadro válido não decodificou"));
            };
            prop_assert_eq!(Envelope::parse(message), Ok(envelope));
        }
    }
}
