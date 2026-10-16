use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::{HEADER_LEN, PayloadTooLarge, decode, encode};

#[derive(Debug)]
pub enum WalError {
    Io(io::Error),
    PayloadTooLarge(PayloadTooLarge),
    /// Uma escrita ou `fsync` anterior falhou. Repetir não é seguro: o kernel pode ter
    /// descartado as páginas sujas e um novo `fsync` "passar" sem gravar nada. O certo é
    /// parar e recuperar pelo log no próximo boot.
    Poisoned,
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "erro de E/S no WAL: {error}"),
            Self::PayloadTooLarge(error) => write!(f, "{error}"),
            Self::Poisoned => write!(f, "WAL envenenado por uma falha anterior de escrita"),
        }
    }
}

impl std::error::Error for WalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::PayloadTooLarge(error) => Some(error),
            Self::Poisoned => None,
        }
    }
}

impl From<io::Error> for WalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PayloadTooLarge> for WalError {
    fn from(error: PayloadTooLarge) -> Self {
        Self::PayloadTooLarge(error)
    }
}

#[derive(Debug)]
pub struct Wal {
    file: File,
    poisoned: bool,
}

#[derive(Debug)]
pub struct Recovery {
    pub wal: Wal,
    pub records: Vec<Vec<u8>>,
    pub discarded: u64,
}

impl Wal {
    /// Abre o log em `path`, criando se não existir, e já recupera: devolve os registros
    /// íntegros e deixa o arquivo cortado no fim do último deles, pronto para anexar.
    ///
    /// # Errors
    /// [`WalError::Io`] se não der para criar, ler, cortar ou sincronizar o arquivo.
    pub fn open(path: &Path) -> Result<Recovery, WalError> {
        let created = !path.try_exists()?;
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        if created {
            sync_parent_dir(path)?;
        }
        let (records, discarded) = recover(&mut file)?;
        Ok(Recovery {
            wal: Self {
                file,
                poisoned: false,
            },
            records,
            discarded,
        })
    }

    /// Anexa um registro e só retorna depois do `fsync`: ao voltar `Ok`, ele sobrevive
    /// a um crash.
    ///
    /// # Errors
    /// [`WalError::PayloadTooLarge`] sem escrever nada; [`WalError::Io`] se a escrita ou
    /// o `fsync` falhar, o que envenena o WAL; [`WalError::Poisoned`] depois disso.
    pub fn append(&mut self, payload: &[u8]) -> Result<(), WalError> {
        self.append_batch(&[payload])
    }

    /// Anexa todos os `payloads`, em ordem, com um único `fsync` (group commit, decisão 5):
    /// ao voltar `Ok`, todos sobrevivem a um crash. Um crash antes disso deixa no disco um
    /// prefixo do lote, que a recuperação corta no último registro inteiro. Lote vazio
    /// não toca o disco.
    ///
    /// # Errors
    /// Os mesmos de [`Wal::append`]; com [`WalError::PayloadTooLarge`], nada do lote é
    /// escrito.
    pub fn append_batch(&mut self, payloads: &[&[u8]]) -> Result<(), WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        if payloads.is_empty() {
            return Ok(());
        }
        let len = payloads
            .iter()
            .map(|payload| HEADER_LEN + payload.len())
            .sum();
        let mut buffer = Vec::with_capacity(len);
        for payload in payloads {
            encode(payload, &mut buffer)?;
        }
        let written = self
            .file
            .write_all(&buffer)
            .and_then(|()| self.file.sync_data());
        if written.is_err() {
            self.poisoned = true;
        }
        Ok(written?)
    }
}

fn recover(file: &mut File) -> io::Result<(Vec<Vec<u8>>, u64)> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let decoded = decode(&bytes);
    let records = decoded.records.into_iter().map(<[u8]>::to_vec).collect();
    let valid_len = u64::try_from(decoded.valid_len).map_err(io::Error::other)?;
    let total = u64::try_from(bytes.len()).map_err(io::Error::other)?;
    let discarded = total - valid_len;
    if discarded > 0 {
        file.set_len(valid_len)?;
        file.sync_data()?;
    }
    Ok((records, discarded))
}

/// O nome de um arquivo novo mora na entrada do diretório; sem sincronizá-lo, um crash
/// pode deixar os dados gravados sem nome nenhum apontando para eles.
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    /// Caminho temporário que se apaga no fim do teste, inclusive se ele falhar.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let file = format!("exchange-wal-{}-{name}", std::process::id());
            let path = std::env::temp_dir().join(file);
            remove_if_present(&path);
            Self(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            remove_if_present(&self.0);
        }
    }

    fn remove_if_present(path: &Path) {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                panic!("não deu para limpar {}: {error}", path.display())
            }
            _ => {}
        }
    }

    fn reopen(path: &TempPath) -> Recovery {
        Wal::open(&path.0).expect("abrir o WAL de teste")
    }

    fn file_len(path: &TempPath) -> u64 {
        fs::metadata(&path.0).expect("metadados do WAL").len()
    }

    fn frame_len(payload: &[u8]) -> u64 {
        u64::try_from(HEADER_LEN + payload.len()).expect("payload de teste é pequeno")
    }

    fn log_with(path: &TempPath, payloads: &[&[u8]]) {
        let mut wal = reopen(path).wal;
        for payload in payloads {
            wal.append(payload).expect("anexar no WAL de teste");
        }
    }

    #[test]
    fn fresh_path_creates_an_empty_log() {
        let path = TempPath::new("fresh");

        let recovery = reopen(&path);

        assert!(path.0.exists());
        assert_eq!(recovery.records, Vec::<Vec<u8>>::new());
        assert_eq!(recovery.discarded, 0);
    }

    #[test]
    fn records_survive_reopening() {
        let path = TempPath::new("reopen");
        log_with(&path, &[b"um", b"dois", b"tres"]);

        let recovery = reopen(&path);

        assert_eq!(
            recovery.records,
            [b"um".to_vec(), b"dois".to_vec(), b"tres".to_vec()]
        );
        assert_eq!(recovery.discarded, 0);
    }

    #[test]
    fn torn_tail_is_cut_and_reported() {
        let path = TempPath::new("torn");
        log_with(&path, &[b"um", b"dois"]);
        let intact = file_len(&path);
        let mut raw = OpenOptions::new().append(true).open(&path.0).unwrap();
        raw.write_all(&[9, 0, 0, 0, 1, 2]).unwrap();

        let recovery = reopen(&path);

        assert_eq!(recovery.records, [b"um".to_vec(), b"dois".to_vec()]);
        assert_eq!(recovery.discarded, 6);
        assert_eq!(file_len(&path), intact);
    }

    #[test]
    fn append_after_recovery_lands_after_the_last_good_record() {
        let path = TempPath::new("append-after");
        log_with(&path, &[b"um"]);
        let mut raw = OpenOptions::new().append(true).open(&path.0).unwrap();
        raw.write_all(&[200, 0, 0, 0, 7]).unwrap();

        reopen(&path).wal.append(b"dois").unwrap();
        let recovery = reopen(&path);

        assert_eq!(recovery.records, [b"um".to_vec(), b"dois".to_vec()]);
        assert_eq!(recovery.discarded, 0);
    }

    #[test]
    fn corrupted_byte_drops_its_record_and_everything_after() {
        let path = TempPath::new("corrupted");
        log_with(&path, &[b"um", b"dois", b"tres"]);
        let mut bytes = fs::read(&path.0).unwrap();
        let second_payload = usize::try_from(frame_len(b"um")).unwrap() + HEADER_LEN;
        bytes[second_payload] ^= 0b0000_0001;
        fs::write(&path.0, &bytes).unwrap();

        let recovery = reopen(&path);

        assert_eq!(recovery.records, [b"um".to_vec()]);
        assert_eq!(recovery.discarded, frame_len(b"dois") + frame_len(b"tres"));
        assert_eq!(file_len(&path), frame_len(b"um"));
    }

    #[test]
    fn failed_write_poisons_the_wal() {
        let path = TempPath::new("poison");
        drop(reopen(&path));
        let read_only = File::open(&path.0).unwrap();
        let mut wal = Wal {
            file: read_only,
            poisoned: false,
        };

        assert!(matches!(wal.append(b"um"), Err(WalError::Io(_))));
        assert!(matches!(wal.append(b"dois"), Err(WalError::Poisoned)));
    }

    #[test]
    fn batch_survives_reopening() {
        let path = TempPath::new("batch");
        reopen(&path)
            .wal
            .append_batch(&[b"um", b"dois", b"tres"])
            .unwrap();

        let recovery = reopen(&path);

        assert_eq!(
            recovery.records,
            [b"um".to_vec(), b"dois".to_vec(), b"tres".to_vec()]
        );
        assert_eq!(recovery.discarded, 0);
    }

    #[test]
    fn batch_writes_the_same_bytes_as_one_append_per_record() {
        let one_by_one = TempPath::new("one-by-one");
        log_with(&one_by_one, &[b"um", b"dois", b"tres"]);
        let batched = TempPath::new("batched");
        reopen(&batched)
            .wal
            .append_batch(&[b"um", b"dois", b"tres"])
            .unwrap();

        assert_eq!(
            fs::read(&batched.0).unwrap(),
            fs::read(&one_by_one.0).unwrap()
        );
    }

    #[test]
    fn records_after_a_batch_land_after_it() {
        let path = TempPath::new("after-batch");
        let mut wal = reopen(&path).wal;
        wal.append_batch(&[b"um", b"dois"]).unwrap();
        wal.append(b"tres").unwrap();
        wal.append_batch(&[b"quatro"]).unwrap();

        let recovery = reopen(&path);

        assert_eq!(
            recovery.records,
            [
                b"um".to_vec(),
                b"dois".to_vec(),
                b"tres".to_vec(),
                b"quatro".to_vec()
            ]
        );
    }

    #[test]
    fn empty_batch_writes_nothing() {
        let path = TempPath::new("empty-batch");
        reopen(&path).wal.append_batch(&[]).unwrap();

        assert_eq!(file_len(&path), 0);
    }

    #[test]
    fn failed_batch_poisons_the_wal() {
        let path = TempPath::new("poison-batch");
        drop(reopen(&path));
        let read_only = File::open(&path.0).unwrap();
        let mut wal = Wal {
            file: read_only,
            poisoned: false,
        };

        assert!(matches!(
            wal.append_batch(&[b"um", b"dois"]),
            Err(WalError::Io(_))
        ));
        assert!(matches!(
            wal.append_batch(&[b"tres"]),
            Err(WalError::Poisoned)
        ));
    }

    // Um crash no meio do `write` do lote deixa no disco só os primeiros bytes dele. Para
    // cada corte possível, a recuperação devolve os registros de antes do lote mais um
    // prefixo do lote — nunca um buraco, nunca um registro pela metade.
    #[test]
    fn crash_at_any_byte_of_a_batch_recovers_a_prefix() {
        let before: [&[u8]; 1] = [b"antes"];
        let batch: [&[u8]; 3] = [b"um", b"dois", b"tres"];
        let path = TempPath::new("batch-crash");
        log_with(&path, &before);
        let start = file_len(&path);
        reopen(&path).wal.append_batch(&batch).unwrap();
        let full = fs::read(&path.0).unwrap();

        let mut ends = Vec::new();
        let mut end = start;
        for payload in batch {
            end += frame_len(payload);
            ends.push(end);
        }

        for cut in start..=file_len(&path) {
            fs::write(&path.0, &full[..usize::try_from(cut).unwrap()]).unwrap();

            let recovery = reopen(&path);

            let complete = ends.iter().take_while(|&&end| end <= cut).count();
            let expected: Vec<Vec<u8>> = before
                .iter()
                .chain(&batch[..complete])
                .map(|payload| payload.to_vec())
                .collect();
            assert_eq!(recovery.records, expected, "corte no byte {cut}");
        }
    }
}
