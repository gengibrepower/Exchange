#![forbid(unsafe_code)]

use std::fmt;

mod file;

pub use file::{Recovery, Wal, WalError};

pub const HEADER_LEN: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PayloadTooLarge {
    pub len: usize,
}

impl fmt::Display for PayloadTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "payload de {} bytes não cabe num u32", self.len)
    }
}

impl std::error::Error for PayloadTooLarge {}

/// # Errors
/// [`PayloadTooLarge`] se o tamanho de `payload` não cabe no campo `u32` do cabeçalho.
pub fn encode(payload: &[u8], out: &mut Vec<u8>) -> Result<(), PayloadTooLarge> {
    let len = u32::try_from(payload.len()).map_err(|_| PayloadTooLarge { len: payload.len() })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&checksum(len, payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

#[derive(PartialEq, Eq, Debug)]
pub struct Decoded<'a> {
    pub records: Vec<&'a [u8]>,
    pub valid_len: usize,
}

pub fn decode(bytes: &[u8]) -> Decoded<'_> {
    let mut records = Vec::new();
    let mut at = 0;
    while let (Some(len), Some(crc)) = (read_u32_le(bytes, at), read_u32_le(bytes, at + 4)) {
        let start = at + HEADER_LEN;
        let Some(payload) = usize::try_from(len)
            .ok()
            .and_then(|l| bytes.get(start..start.checked_add(l)?))
        else {
            break;
        };
        if checksum(len, payload) != crc {
            break;
        }
        records.push(payload);
        at = start + payload.len();
    }
    Decoded {
        records,
        valid_len: at,
    }
}

/// O crc cobre o tamanho junto com o payload. Só o payload não bastaria: o crc32 de zero
/// bytes é 0, então um rabo de zeros (arquivo estendido pelo sistema de arquivos sem os
/// dados terem chegado ao disco) passaria como uma fileira de registros vazios válidos.
fn checksum(len: u32, payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&len.to_le_bytes());
    hasher.update(payload);
    hasher.finalize()
}

fn read_u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(field.try_into().ok()?))
}

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;

    fn log_of(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for payload in payloads {
            encode(payload, &mut out).expect("payload de teste é pequeno");
        }
        out
    }

    fn nothing() -> Decoded<'static> {
        Decoded {
            records: Vec::new(),
            valid_len: 0,
        }
    }

    #[test]
    fn encode_writes_little_endian_length_crc_and_payload() {
        let bytes = log_of(&[b"abc"]);
        assert_eq!(&bytes[0..4], &3u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &checksum(3, b"abc").to_le_bytes());
        assert_eq!(&bytes[8..], b"abc");
    }

    #[test]
    fn empty_log_has_no_records() {
        assert_eq!(decode(&[]), nothing());
    }

    #[test]
    fn decodes_records_in_order() {
        let bytes = log_of(&[b"um", b"dois", b"tres"]);
        let decoded = decode(&bytes);
        assert_eq!(decoded.records, vec![&b"um"[..], b"dois", b"tres"]);
        assert_eq!(decoded.valid_len, bytes.len());
    }

    #[test]
    fn empty_payload_is_a_valid_record() {
        let bytes = log_of(&[b"", b"x"]);
        let decoded = decode(&bytes);
        assert_eq!(decoded.records, vec![&b""[..], b"x"]);
        assert_eq!(decoded.valid_len, bytes.len());
    }

    #[test]
    fn truncated_header_is_discarded() {
        let mut bytes = log_of(&[b"inteiro"]);
        let intact = bytes.len();
        bytes.extend_from_slice(&[7, 0, 0]);
        let decoded = decode(&bytes);
        assert_eq!(decoded.records, vec![&b"inteiro"[..]]);
        assert_eq!(decoded.valid_len, intact);
    }

    #[test]
    fn truncated_payload_is_discarded() {
        let first = log_of(&[b"inteiro"]);
        let bytes = log_of(&[b"inteiro", b"rasgado"]);
        let decoded = decode(&bytes[..bytes.len() - 1]);
        assert_eq!(decoded.records, vec![&b"inteiro"[..]]);
        assert_eq!(decoded.valid_len, first.len());
    }

    #[test]
    fn corrupted_payload_stops_decoding_even_if_later_records_are_intact() {
        let first = log_of(&[b"bom"]);
        let mut bytes = log_of(&[b"bom", b"ruim", b"bom de novo"]);
        bytes[first.len() + HEADER_LEN] ^= 0b0000_0001;
        let decoded = decode(&bytes);
        assert_eq!(decoded.records, vec![&b"bom"[..]]);
        assert_eq!(decoded.valid_len, first.len());
    }

    #[test]
    fn corrupted_crc_stops_decoding() {
        let mut bytes = log_of(&[b"so"]);
        bytes[4] ^= 0b1000_0000;
        assert_eq!(decode(&bytes), nothing());
    }

    #[test]
    fn checksum_covers_the_length_so_zeroed_header_is_not_a_record() {
        assert_ne!(checksum(0, b""), 0);
    }

    #[test]
    fn zeroed_tail_is_discarded() {
        let mut bytes = log_of(&[b"inteiro"]);
        let intact = bytes.len();
        bytes.extend_from_slice(&[0; 64]);
        let decoded = decode(&bytes);
        assert_eq!(decoded.records, vec![&b"inteiro"[..]]);
        assert_eq!(decoded.valid_len, intact);
    }

    #[test]
    fn corrupted_length_is_caught_by_the_checksum() {
        let mut bytes = log_of(&[b"abcde", b"seguinte"]);
        bytes[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(decode(&bytes), nothing());
    }

    #[test]
    fn length_pointing_past_the_end_is_discarded() {
        let mut bytes = log_of(&[b"curto"]);
        bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&bytes), nothing());
    }
}
