use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{InstrumentId, Seq};
use crate::engine::Command;

/// A quem o registro pertence no log global (decisão 10c): permite separar o log em
/// logs por partição no futuro sem reescrever o formato.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Partition {
    Ledger,
    Instrument(InstrumentId),
}

impl Command {
    pub fn partition(self) -> Partition {
        match self {
            Self::Deposit { .. } => Partition::Ledger,
            Self::Submit(request) => Partition::Instrument(request.instrument),
            Self::Cancel { instrument, .. } => Partition::Instrument(instrument),
        }
    }
}

/// Um comando como ele vai para o WAL. A partição é derivada do comando, nunca
/// informada: só se constrói por [`LogRecord::new`] ou lendo bytes que concordam.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawRecord")]
pub struct LogRecord {
    seq: Seq,
    partition: Partition,
    command: Command,
}

/// O registro como saiu do disco, antes de conferir que a partição bate com o comando.
#[derive(Deserialize)]
struct RawRecord {
    seq: Seq,
    partition: Partition,
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartitionMismatch {
    pub recorded: Partition,
    pub expected: Partition,
}

impl fmt::Display for PartitionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "partição gravada {:?} não bate com a do comando, {:?}",
            self.recorded, self.expected
        )
    }
}

impl std::error::Error for PartitionMismatch {}

impl TryFrom<RawRecord> for LogRecord {
    type Error = PartitionMismatch;

    fn try_from(raw: RawRecord) -> Result<Self, Self::Error> {
        let expected = raw.command.partition();
        if raw.partition != expected {
            return Err(PartitionMismatch {
                recorded: raw.partition,
                expected,
            });
        }
        Ok(Self {
            seq: raw.seq,
            partition: raw.partition,
            command: raw.command,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// Bytes que não formam um registro: truncados, variante inexistente, newtype
    /// inválido (`Price` zero, `Amount` negativo) ou partição divergente.
    Malformed(postcard::Error),
    /// Um quadro do WAL guarda exatamente um registro; sobra é corrupção.
    TrailingBytes(usize),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(error) => write!(f, "registro malformado: {error}"),
            Self::TrailingBytes(count) => write!(f, "{count} bytes sobrando após o registro"),
        }
    }
}

impl std::error::Error for RecordError {}

impl LogRecord {
    pub fn new(seq: Seq, command: Command) -> Self {
        Self {
            seq,
            partition: command.partition(),
            command,
        }
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }

    pub fn partition(&self) -> Partition {
        self.partition
    }

    pub fn command(&self) -> Command {
        self.command
    }

    /// # Panics
    /// Nunca: o registro só tem inteiros e enums, que o postcard sempre serializa num
    /// `Vec`. Falha de serialização aqui seria erro de programação, não de entrada.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("registro só tem inteiros e enums")
    }

    /// # Errors
    /// [`RecordError::Malformed`] se os bytes não formam um registro válido;
    /// [`RecordError::TrailingBytes`] se sobram bytes depois dele.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RecordError> {
        let (record, rest) =
            postcard::take_from_bytes::<Self>(bytes).map_err(RecordError::Malformed)?;
        if !rest.is_empty() {
            return Err(RecordError::TrailingBytes(rest.len()));
        }
        Ok(record)
    }
}

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AccountId, Amount, AssetId, Lots, OrderId, OrderKind, OrderRequest, Price,
    };
    use crate::domain::{Side, TimeInForce};

    fn deposit() -> Command {
        Command::Deposit {
            account: AccountId::new(1),
            asset: AssetId::new(2),
            amount: Amount::new(300).expect("quantia de teste válida"),
        }
    }

    fn submit(instrument: u32) -> Command {
        Command::Submit(OrderRequest {
            id: OrderId::new(5),
            account: AccountId::new(1),
            instrument: InstrumentId::new(instrument),
            side: Side::Bid,
            kind: OrderKind::Limit(Price::new(100).expect("preço de teste válido")),
            total: Lots::new(3).expect("lots de teste válidos"),
            seq: Seq::new(5),
            time_in_force: TimeInForce::Gtc,
        })
    }

    #[test]
    fn deposit_belongs_to_the_ledger() {
        assert_eq!(deposit().partition(), Partition::Ledger);
    }

    #[test]
    fn submit_and_cancel_belong_to_their_instrument() {
        let instrument = InstrumentId::new(4);
        assert_eq!(submit(4).partition(), Partition::Instrument(instrument));
        let cancel = Command::Cancel {
            instrument,
            account: AccountId::new(1),
            order: OrderId::new(5),
        };
        assert_eq!(cancel.partition(), Partition::Instrument(instrument));
    }

    #[test]
    fn new_derives_the_partition_from_the_command() {
        let record = LogRecord::new(Seq::new(1), submit(4));
        assert_eq!(
            record.partition(),
            Partition::Instrument(InstrumentId::new(4))
        );
    }

    // Foto do formato em disco. Se este teste quebrar, logs já gravados deixam de ser
    // lidos: mudar a ordem de campos ou variantes é mudança de formato, não refactor.
    #[test]
    fn deposit_record_has_a_fixed_byte_layout() {
        let record = LogRecord::new(Seq::new(7), deposit());

        // seq 7 | Ledger | Deposit | conta 1 | ativo 2 | quantia 300 (zigzag 600, varint)
        assert_eq!(record.to_bytes(), [7, 0, 0, 1, 2, 0xD8, 0x04]);
    }

    #[test]
    fn roundtrip_of_a_known_record() {
        let record = LogRecord::new(Seq::new(9), submit(1));
        assert_eq!(LogRecord::from_bytes(&record.to_bytes()), Ok(record));
    }

    // Os testes abaixo montam bytes à mão, imitando o layout campo a campo: é a única
    // forma de produzir no disco um valor que os construtores do domínio não deixam existir.

    #[test]
    fn negative_amount_in_bytes_is_rejected() {
        // seq | Ledger | Deposit | conta | ativo | quantia -5
        let bytes = postcard::to_allocvec(&(7u64, 0u32, 0u32, 1u64, 2u32, -5i64)).unwrap();
        assert!(matches!(
            LogRecord::from_bytes(&bytes),
            Err(RecordError::Malformed(_))
        ));
    }

    #[test]
    fn zero_price_in_bytes_is_rejected() {
        // seq | Instrument(1) | Submit | id | conta | instrumento | Bid | Limit(0) | lots | seq | Gtc
        let fields = (
            7u64, 1u32, 1u32, 1u32, 5u64, 1u64, 1u32, 0u32, 0u32, 0i64, 3i64, 5u64, 0u32,
        );
        let bytes = postcard::to_allocvec(&fields).unwrap();
        assert!(matches!(
            LogRecord::from_bytes(&bytes),
            Err(RecordError::Malformed(_))
        ));
    }

    #[test]
    fn partition_disagreeing_with_the_command_is_rejected() {
        // seq | Instrument(1) | Deposit | conta | ativo | quantia 300
        let bytes = postcard::to_allocvec(&(7u64, 1u32, 1u32, 0u32, 1u64, 2u32, 300i64)).unwrap();
        assert!(matches!(
            LogRecord::from_bytes(&bytes),
            Err(RecordError::Malformed(_))
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = LogRecord::new(Seq::new(7), deposit()).to_bytes();
        bytes.push(0);
        assert_eq!(
            LogRecord::from_bytes(&bytes),
            Err(RecordError::TrailingBytes(1))
        );
    }

    #[test]
    fn truncated_record_is_rejected() {
        let bytes = LogRecord::new(Seq::new(7), deposit()).to_bytes();
        assert!(matches!(
            LogRecord::from_bytes(&bytes[..bytes.len() - 1]),
            Err(RecordError::Malformed(_))
        ));
    }
}
