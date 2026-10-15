use proptest::prelude::*;

use super::*;

fn tag() -> impl Strategy<Value = Tag> {
    (1..=u32::MAX).prop_filter_map("tags do envelope ficam fora do corpo", |number| {
        Tag::new(number).filter(|tag| !tag.is_reserved())
    })
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        any::<u8>().prop_filter("SOH delimita campos", |&b| b != SOH),
        1..20,
    )
}

fn message() -> impl Strategy<Value = Message> {
    let msg_type = prop::collection::vec(prop::char::range('0', 'Z'), 1..3)
        .prop_map(|chars| chars.into_iter().collect::<String>());
    let field = (tag(), value())
        .prop_map(|(tag, value)| Field::new(tag, value).expect("valor sem SOH e não vazio"));
    (msg_type, prop::collection::vec(field, 0..10)).prop_map(|(msg_type, body)| {
        Message::new(msg_type, body).expect("estratégia só gera mensagens válidas")
    })
}

fn encoded(message: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    encode(message, &mut out).expect("estratégia fica abaixo do teto");
    out
}

proptest! {
    #[test]
    fn decode_inverts_encode(message in message()) {
        let bytes = encoded(&message);
        prop_assert_eq!(
            decode(&bytes),
            Frame::Complete { message, consumed: bytes.len() }
        );
    }

    /// Simula o TCP: as mensagens chegam grudadas e fatiadas em pedaços arbitrários, e o
    /// buffer da conexão é drenado a cada quadro completo.
    #[test]
    fn stream_split_anywhere_yields_the_same_messages(
        messages in prop::collection::vec(message(), 1..5),
        cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..10),
    ) {
        let stream: Vec<u8> = messages.iter().flat_map(encoded).collect();
        let mut cuts: Vec<usize> = cuts.iter().map(|cut| cut.index(stream.len())).collect();
        cuts.push(stream.len());
        cuts.sort_unstable();

        let mut buffer = Vec::new();
        let mut received = Vec::new();
        let mut from = 0;
        for cut in cuts {
            buffer.extend_from_slice(&stream[from..cut]);
            from = cut;
            loop {
                match decode(&buffer) {
                    Frame::Complete { message, consumed } => {
                        received.push(message);
                        buffer.drain(..consumed);
                    }
                    Frame::Incomplete => break,
                    Frame::Garbled { reason, .. } => {
                        return Err(TestCaseError::fail(format!("quadro válido rejeitado: {reason}")));
                    }
                }
            }
        }
        prop_assert_eq!(received, messages);
        prop_assert!(buffer.is_empty());
    }

    #[test]
    fn single_corrupted_byte_never_decodes(
        message in message(),
        at in any::<prop::sample::Index>(),
        replacement in any::<u8>(),
    ) {
        let mut bytes = encoded(&message);
        let at = at.index(bytes.len());
        prop_assume!(bytes[at] != replacement);
        bytes[at] = replacement;
        let frame = decode(&bytes);
        prop_assert!(!matches!(frame, Frame::Complete { .. }), "{frame:?}");
    }

    /// Entrada da rede é arbitrária: decode nunca entra em pânico e sempre faz o buffer
    /// andar, senão a conexão travaria no mesmo lixo para sempre.
    #[test]
    fn arbitrary_bytes_never_panic_and_always_make_progress(
        bytes in prop::collection::vec(any::<u8>(), 0..200),
    ) {
        match decode(&bytes) {
            Frame::Complete { consumed, .. } => {
                prop_assert!(consumed > 0 && consumed <= bytes.len());
            }
            Frame::Garbled { skip, .. } => {
                prop_assert!(skip > 0 && skip <= bytes.len());
            }
            Frame::Incomplete => {}
        }
    }
}
