use proptest::prelude::*;

use super::{decode, encode};

fn payloads() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(prop::collection::vec(any::<u8>(), 0..32), 0..12)
}

fn encode_all(payloads: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut ends = Vec::new();
    for payload in payloads {
        encode(payload, &mut bytes).expect("payload de teste é pequeno");
        ends.push(bytes.len());
    }
    (bytes, ends)
}

proptest! {
    #[test]
    fn roundtrip(payloads in payloads()) {
        let (bytes, _) = encode_all(&payloads);
        let decoded = decode(&bytes);
        prop_assert_eq!(decoded.records, payloads.iter().map(Vec::as_slice).collect::<Vec<_>>());
        prop_assert_eq!(decoded.valid_len, bytes.len());
    }

    // Simula um crash que deixou no disco só os primeiros `cut` bytes: sobrevivem
    // exatamente os registros que terminaram antes do corte.
    #[test]
    fn truncation_at_any_byte_keeps_only_complete_records(
        (payloads, cut) in payloads().prop_flat_map(|p| {
            let len = encode_all(&p).0.len();
            (Just(p), 0..=len)
        })
    ) {
        let (bytes, ends) = encode_all(&payloads);
        let complete = ends.iter().take_while(|&&end| end <= cut).count();
        let decoded = decode(&bytes[..cut]);
        prop_assert_eq!(
            decoded.records,
            payloads[..complete].iter().map(Vec::as_slice).collect::<Vec<_>>()
        );
        prop_assert_eq!(decoded.valid_len, if complete == 0 { 0 } else { ends[complete - 1] });
    }

    #[test]
    fn flipped_bit_drops_its_record_and_everything_after(
        (payloads, position, bit) in payloads()
            .prop_filter("precisa de pelo menos um byte", |p| !p.is_empty())
            .prop_flat_map(|p| {
                let len = encode_all(&p).0.len();
                (Just(p), 0..len, 0..8u8)
            })
    ) {
        let (mut bytes, ends) = encode_all(&payloads);
        bytes[position] ^= 1 << bit;
        let intact = ends.iter().take_while(|&&end| end <= position).count();
        let decoded = decode(&bytes);
        prop_assert_eq!(
            decoded.records,
            payloads[..intact].iter().map(Vec::as_slice).collect::<Vec<_>>()
        );
    }
}
