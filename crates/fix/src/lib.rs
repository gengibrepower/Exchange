#![forbid(unsafe_code)]

use std::fmt;
use std::num::NonZeroU32;

mod admin;
mod session;
mod time;

pub use admin::{
    Admin, AppMessage, Body, Envelope, Header, NotApp, ParseError, RejectReason, SeqNum,
    SessionReject, Text,
};
pub use session::{Action, Role, Session, SessionConfig};
pub use time::UtcTimestamp;

pub const SOH: u8 = 0x01;
pub const MAX_BODY_LEN: usize = 4096;

const BEGIN_STRING: &[u8] = b"FIX.4.4";
const FRAME_PREFIX: &[u8] = b"8=FIX.4.4\x019=";
const TRAILER_LEN: usize = b"10=000\x01".len();

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tag(NonZeroU32);

impl Tag {
    pub const BEGIN_STRING: Self = Self::known(8);
    pub const BODY_LENGTH: Self = Self::known(9);
    pub const CHECKSUM: Self = Self::known(10);
    pub const MSG_TYPE: Self = Self::known(35);

    pub const fn new(number: u32) -> Option<Self> {
        match NonZeroU32::new(number) {
            Some(number) => Some(Self(number)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Só usada para inicializar constantes: o `panic!` acontece em tempo de compilação,
    /// nunca em execução, então nenhuma entrada consegue alcançá-lo.
    const fn known(number: u32) -> Self {
        match Self::new(number) {
            Some(tag) => tag,
            None => panic!("tag FIX nunca é zero"),
        }
    }

    fn is_reserved(self) -> bool {
        [
            Self::BEGIN_STRING,
            Self::BODY_LENGTH,
            Self::CHECKSUM,
            Self::MSG_TYPE,
        ]
        .contains(&self)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidValue {
    Empty,
    ContainsSoh,
}

impl fmt::Display for InvalidValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("valor vazio"),
            Self::ContainsSoh => f.write_str("valor contém SOH"),
        }
    }
}

impl std::error::Error for InvalidValue {}

fn validate_value(value: &[u8]) -> Result<(), InvalidValue> {
    if value.is_empty() {
        Err(InvalidValue::Empty)
    } else if value.contains(&SOH) {
        Err(InvalidValue::ContainsSoh)
    } else {
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Field {
    tag: Tag,
    value: Vec<u8>,
}

impl Field {
    /// # Errors
    /// [`InvalidValue`] se o valor é vazio ou contém SOH, que é o delimitador de campos.
    pub fn new(tag: Tag, value: impl Into<Vec<u8>>) -> Result<Self, InvalidValue> {
        let value = value.into();
        validate_value(&value)?;
        Ok(Self { tag, value })
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidMessage {
    MsgType(InvalidValue),
    ReservedTag(Tag),
}

impl fmt::Display for InvalidMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MsgType(reason) => write!(f, "MsgType inválido: {reason}"),
            Self::ReservedTag(tag) => write!(f, "tag {tag} é do cabeçalho ou do trailer"),
        }
    }
}

impl std::error::Error for InvalidMessage {}

/// Mensagem sem o envelope: `8`, `9` e `10` são do codec, e o `35` vem separado do corpo
/// porque é obrigatório e sempre o primeiro. A ordem dos campos do corpo é preservada,
/// já que no FIX a posição importa (grupos repetidos repetem tags).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Message {
    msg_type: Vec<u8>,
    body: Vec<Field>,
}

impl Message {
    /// # Errors
    /// [`InvalidMessage`] se o `MsgType` é inválido ou se o corpo usa uma tag do envelope.
    pub fn new(msg_type: impl Into<Vec<u8>>, body: Vec<Field>) -> Result<Self, InvalidMessage> {
        let msg_type = msg_type.into();
        validate_value(&msg_type).map_err(InvalidMessage::MsgType)?;
        if let Some(field) = body.iter().find(|field| field.tag.is_reserved()) {
            return Err(InvalidMessage::ReservedTag(field.tag));
        }
        Ok(Self { msg_type, body })
    }

    pub fn msg_type(&self) -> &[u8] {
        &self.msg_type
    }

    pub fn body(&self) -> &[Field] {
        &self.body
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BodyTooLong {
    pub len: usize,
}

impl fmt::Display for BodyTooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "corpo de {} bytes passa do teto de {MAX_BODY_LEN}",
            self.len
        )
    }
}

impl std::error::Error for BodyTooLong {}

/// # Errors
/// [`BodyTooLong`] se o corpo passa de [`MAX_BODY_LEN`]: o outro lado o descartaria.
pub fn encode(message: &Message, out: &mut Vec<u8>) -> Result<(), BodyTooLong> {
    let mut body = Vec::new();
    push_field(&mut body, Tag::MSG_TYPE, &message.msg_type);
    for field in &message.body {
        push_field(&mut body, field.tag, &field.value);
    }
    if body.len() > MAX_BODY_LEN {
        return Err(BodyTooLong { len: body.len() });
    }

    let start = out.len();
    push_field(out, Tag::BEGIN_STRING, BEGIN_STRING);
    push_field(out, Tag::BODY_LENGTH, body.len().to_string().as_bytes());
    out.extend_from_slice(&body);
    let sum = checksum(&out[start..]);
    push_field(out, Tag::CHECKSUM, format!("{sum:03}").as_bytes());
    Ok(())
}

fn push_field(out: &mut Vec<u8>, tag: Tag, value: &[u8]) {
    out.extend_from_slice(tag.to_string().as_bytes());
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(SOH);
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Frame {
    Complete { message: Message, consumed: usize },
    Incomplete,
    Garbled { reason: Garbled, skip: usize },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Garbled {
    BadBeginString,
    BadBodyLength,
    BodyTooLong,
    BadTrailer,
    ChecksumMismatch { declared: u8, computed: u8 },
    BadField,
    MissingMsgType,
    ReservedTag(Tag),
}

impl fmt::Display for Garbled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadBeginString => f.write_str("não começa com 8=FIX.4.4"),
            Self::BadBodyLength => f.write_str("BodyLength ausente ou malformado"),
            Self::BodyTooLong => write!(f, "BodyLength passa do teto de {MAX_BODY_LEN}"),
            Self::BadTrailer => f.write_str("10=ddd não está onde o BodyLength promete"),
            Self::ChecksumMismatch { declared, computed } => {
                write!(
                    f,
                    "checksum declarado {declared:03}, calculado {computed:03}"
                )
            }
            Self::BadField => f.write_str("campo malformado"),
            Self::MissingMsgType => f.write_str("primeiro campo do corpo não é o 35"),
            Self::ReservedTag(tag) => write!(f, "tag {tag} fora do lugar"),
        }
    }
}

impl std::error::Error for Garbled {}

pub fn decode(bytes: &[u8]) -> Frame {
    let garbled = |reason| Frame::Garbled {
        reason,
        skip: resync(bytes, 1),
    };

    let Some(after_prefix) = bytes.strip_prefix(FRAME_PREFIX) else {
        return if FRAME_PREFIX.starts_with(bytes) {
            Frame::Incomplete
        } else {
            garbled(Garbled::BadBeginString)
        };
    };

    let Some(length_end) = after_prefix.iter().position(|&byte| byte == SOH) else {
        return match body_length(after_prefix) {
            Err(reason) if !after_prefix.is_empty() => garbled(reason),
            _ => Frame::Incomplete,
        };
    };
    let body_len = match body_length(&after_prefix[..length_end]) {
        Ok(len) => len,
        Err(reason) => return garbled(reason),
    };
    let body_start = FRAME_PREFIX.len() + length_end + 1;

    let body_end = body_start + body_len;
    let Some(trailer) = bytes.get(body_end..body_end + TRAILER_LEN) else {
        return Frame::Incomplete;
    };
    let Some(declared) = trailer_checksum(trailer) else {
        return garbled(Garbled::BadTrailer);
    };
    let Some(fields) = bytes[body_start..body_end].strip_suffix(&[SOH]) else {
        return garbled(Garbled::BadTrailer);
    };

    let computed = checksum(&bytes[..body_end]);
    if declared != computed {
        return garbled(Garbled::ChecksumMismatch { declared, computed });
    }

    let parsed = match fields
        .split(|&byte| byte == SOH)
        .map(parse_field)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(parsed) => parsed,
        Err(reason) => return garbled(reason),
    };

    let mut parsed = parsed.into_iter();
    let Some(msg_type) = parsed.next().filter(|field| field.tag() == Tag::MSG_TYPE) else {
        return garbled(Garbled::MissingMsgType);
    };

    match Message::new(msg_type.value, parsed.collect()) {
        Ok(message) => Frame::Complete {
            message,
            consumed: body_end + TRAILER_LEN,
        },
        Err(InvalidMessage::ReservedTag(tag)) => garbled(Garbled::ReservedTag(tag)),
        Err(InvalidMessage::MsgType(_)) => garbled(Garbled::BadField),
    }
}

/// Onde pode começar o próximo quadro, a partir de `from`: o início de um
/// `8=FIX.4.4␁9=` inteiro, ou de um pedaço dele cortado no fim do buffer, que ainda pode
/// se completar com os próximos bytes. Sem nenhum dos dois, descarta tudo.
fn resync(bytes: &[u8], from: usize) -> usize {
    (from..bytes.len())
        .find(|&at| {
            let rest = &bytes[at..];
            rest.starts_with(FRAME_PREFIX) || FRAME_PREFIX.starts_with(rest)
        })
        .unwrap_or(bytes.len())
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, &byte| sum.wrapping_add(byte))
}

fn parse_field(raw: &[u8]) -> Result<Field, Garbled> {
    let equals = raw
        .iter()
        .position(|&byte| byte == b'=')
        .ok_or(Garbled::BadField)?;
    let tag = parse_number(&raw[..equals])
        .and_then(|number| u32::try_from(number).ok())
        .and_then(Tag::new)
        .ok_or(Garbled::BadField)?;
    Field::new(tag, &raw[equals + 1..]).map_err(|_| Garbled::BadField)
}

fn parse_number(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || (digits.starts_with(b"0") && digits.len() > 1) {
        return None;
    }
    digits.iter().try_fold(0u64, |number, &raw| {
        if !raw.is_ascii_digit() {
            return None;
        }
        number.checked_mul(10)?.checked_add(u64::from(raw - b'0'))
    })
}

fn body_length(digits: &[u8]) -> Result<usize, Garbled> {
    let len = parse_number(digits).ok_or(Garbled::BadBodyLength)?;
    let len = usize::try_from(len).map_err(|_| Garbled::BodyTooLong)?;
    if len > MAX_BODY_LEN {
        Err(Garbled::BodyTooLong)
    } else {
        Ok(len)
    }
}

fn trailer_checksum(trailer: &[u8]) -> Option<u8> {
    let &[b'1', b'0', b'=', hundreds, tens, units, SOH] = trailer else {
        return None;
    };
    if ![hundreds, tens, units].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value =
        u32::from(hundreds - b'0') * 100 + u32::from(tens - b'0') * 10 + u32::from(units - b'0');
    u8::try_from(value).ok()
}

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(number: u32) -> Tag {
        Tag::new(number).expect("tag de teste não é zero")
    }

    fn order() -> Message {
        Message::new(
            "D",
            vec![
                Field::new(tag(11), "ord-1").expect("valor de teste válido"),
                Field::new(tag(54), "1").expect("valor de teste válido"),
            ],
        )
        .expect("mensagem de teste válida")
    }

    fn encoded(message: &Message) -> Vec<u8> {
        let mut out = Vec::new();
        encode(message, &mut out).expect("mensagem de teste cabe no teto");
        out
    }

    fn soh(text: &str) -> Vec<u8> {
        text.replace('|', "\x01").into_bytes()
    }

    /// Monta um quadro com `BodyLength` e `CheckSum` corretos para o corpo dado, que pode ser
    /// inválido: é como os testes alcançam os erros que vêm depois do enquadramento.
    fn framed(body: &str) -> Vec<u8> {
        let body = soh(body);
        let mut out = soh(&format!("8=FIX.4.4|9={}|", body.len()));
        out.extend_from_slice(&body);
        let sum = checksum(&out);
        out.extend_from_slice(&soh(&format!("10={sum:03}|")));
        out
    }

    fn garbled(bytes: &[u8]) -> Garbled {
        match decode(bytes) {
            Frame::Garbled { reason, .. } => reason,
            other => panic!("esperava Garbled, veio {other:?}"),
        }
    }

    #[test]
    fn tag_zero_does_not_exist() {
        assert_eq!(Tag::new(0), None);
    }

    #[test]
    fn field_rejects_empty_value_and_soh() {
        assert_eq!(Field::new(tag(11), ""), Err(InvalidValue::Empty));
        assert_eq!(
            Field::new(tag(11), "a\x01b"),
            Err(InvalidValue::ContainsSoh)
        );
    }

    #[test]
    fn message_rejects_envelope_tags_in_body() {
        for reserved in [8, 9, 10, 35] {
            let field = Field::new(tag(reserved), "x").expect("valor de teste válido");
            assert_eq!(
                Message::new("D", vec![field]),
                Err(InvalidMessage::ReservedTag(tag(reserved)))
            );
        }
    }

    #[test]
    fn checksum_is_byte_sum_modulo_256() {
        assert_eq!(checksum(b""), 0);
        assert_eq!(checksum(&[1, 2, 3]), 6);
        assert_eq!(checksum(&[200, 100]), 44);
    }

    #[test]
    fn parse_field_splits_at_the_first_equals_sign() {
        let field = Field::new(tag(58), "a=b").expect("valor de teste válido");
        assert_eq!(parse_field(b"58=a=b"), Ok(field));
        assert_eq!(parse_field(b"58"), Err(Garbled::BadField));
        assert_eq!(parse_field(b"58="), Err(Garbled::BadField));
        assert_eq!(parse_field(b"058=x"), Err(Garbled::BadField));
        assert_eq!(parse_field(b"5a=x"), Err(Garbled::BadField));
    }

    #[test]
    fn encode_wraps_body_in_begin_string_length_and_checksum() {
        let bytes = encoded(&order());
        let body = soh("35=D|11=ord-1|54=1|");
        let mut expected = soh(&format!("8=FIX.4.4|9={}|", body.len()));
        expected.extend_from_slice(&body);
        let sum = checksum(&expected);
        expected.extend_from_slice(&soh(&format!("10={sum:03}|")));
        assert_eq!(bytes, expected);
    }

    #[test]
    fn encode_refuses_body_over_the_limit() {
        let huge = Field::new(tag(58), vec![b'x'; MAX_BODY_LEN]).expect("valor de teste válido");
        let message = Message::new("D", vec![huge]).expect("mensagem de teste válida");
        assert!(encode(&message, &mut Vec::new()).is_err());
    }

    #[test]
    fn decodes_what_was_encoded() {
        let bytes = encoded(&order());
        assert_eq!(
            decode(&bytes),
            Frame::Complete {
                message: order(),
                consumed: bytes.len()
            }
        );
    }

    #[test]
    fn decodes_only_the_first_of_two_glued_messages() {
        let one = encoded(&order());
        let mut two = one.clone();
        two.extend_from_slice(&one);
        assert_eq!(
            decode(&two),
            Frame::Complete {
                message: order(),
                consumed: one.len()
            }
        );
    }

    #[test]
    fn every_proper_prefix_is_incomplete() {
        let bytes = encoded(&order());
        for len in 0..bytes.len() {
            assert_eq!(decode(&bytes[..len]), Frame::Incomplete, "prefixo de {len}");
        }
    }

    #[test]
    fn rejects_other_begin_string() {
        assert_eq!(
            garbled(&soh("8=FIX.4.2|9=5|35=D|10=000|")),
            Garbled::BadBeginString
        );
    }

    #[test]
    fn rejects_malformed_body_length() {
        assert_eq!(garbled(&soh("8=FIX.4.4|9=|")), Garbled::BadBodyLength);
        assert_eq!(garbled(&soh("8=FIX.4.4|9=x5|")), Garbled::BadBodyLength);
        assert_eq!(garbled(&soh("8=FIX.4.4|9=05|")), Garbled::BadBodyLength);
    }

    #[test]
    fn rejects_body_length_over_the_limit_without_waiting_for_it() {
        let declared = format!("8=FIX.4.4|9={}|", MAX_BODY_LEN + 1);
        assert_eq!(garbled(&soh(&declared)), Garbled::BodyTooLong);
    }

    #[test]
    fn rejects_trailer_away_from_where_body_length_points() {
        let mut bytes = encoded(&order());
        let last_body_len_digit = FRAME_PREFIX.len()
            + bytes[FRAME_PREFIX.len()..]
                .iter()
                .position(|&b| b == SOH)
                .expect("o BodyLength termina em SOH")
            - 1;
        bytes[last_body_len_digit] -= 1;
        assert_eq!(garbled(&bytes), Garbled::BadTrailer);
    }

    #[test]
    fn rejects_wrong_checksum() {
        let mut bytes = encoded(&order());
        let ord_byte = bytes
            .windows(5)
            .position(|w| w == b"ord-1")
            .expect("o valor está no quadro");
        bytes[ord_byte] = b'O';
        assert!(matches!(garbled(&bytes), Garbled::ChecksumMismatch { .. }));
    }

    #[test]
    fn rejects_malformed_fields() {
        assert_eq!(garbled(&framed("35=D|11|")), Garbled::BadField);
        assert_eq!(garbled(&framed("35=D|=x|")), Garbled::BadField);
        assert_eq!(garbled(&framed("35=D|011=x|")), Garbled::BadField);
        assert_eq!(garbled(&framed("35=D|0=x|")), Garbled::BadField);
        assert_eq!(garbled(&framed("35=D|11=|")), Garbled::BadField);
        assert_eq!(garbled(&framed("35=D|99999999999=x|")), Garbled::BadField);
    }

    #[test]
    fn rejects_body_not_starting_with_msg_type() {
        assert_eq!(garbled(&framed("11=x|35=D|")), Garbled::MissingMsgType);
    }

    #[test]
    fn rejects_envelope_tag_inside_body() {
        assert_eq!(
            garbled(&framed("35=D|10=123|")),
            Garbled::ReservedTag(tag(10))
        );
    }

    #[test]
    fn garbled_skips_to_the_next_frame() {
        let mut bytes = soh("lixo|");
        let good = encoded(&order());
        bytes.extend_from_slice(&good);
        assert_eq!(
            decode(&bytes),
            Frame::Garbled {
                reason: Garbled::BadBeginString,
                skip: bytes.len() - good.len()
            }
        );
    }

    #[test]
    fn resync_keeps_a_frame_start_cut_at_the_end() {
        let bytes = soh("lixo|8=FI");
        assert_eq!(resync(&bytes, 1), b"lixo\x01".len());
    }
}
