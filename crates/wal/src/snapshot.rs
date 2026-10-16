//! Um snapshot é um arquivo só, trocado inteiro (decisão 6): o conteúdo vai para um
//! temporário, que é sincronizado e só então renomeado por cima do anterior. O `rename` é
//! atômico, então um crash em qualquer ponto deixa no caminho o snapshot antigo inteiro ou
//! o novo inteiro.
//!
//! Formato: `[MAGIC][tamanho][crc32][payload]`, o quadro do WAL (decisão 5) depois de uma
//! marca que distingue um snapshot de um log com um registro só.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::file::sync_parent_dir;
use crate::{PayloadTooLarge, decode, encode};

/// O `01` é a versão do formato.
const MAGIC: &[u8; 8] = b"EXSNAP01";

#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    PayloadTooLarge(PayloadTooLarge),
    /// Não começa com a marca: outro arquivo no caminho, ou outra versão do formato.
    NotASnapshot,
    /// A marca está lá, mas o quadro não fecha: truncado, crc errado ou bytes sobrando.
    Damaged,
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "erro de E/S no snapshot: {error}"),
            Self::PayloadTooLarge(error) => write!(f, "{error}"),
            Self::NotASnapshot => write!(f, "o arquivo não é um snapshot desta versão"),
            Self::Damaged => write!(f, "snapshot danificado"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::PayloadTooLarge(error) => Some(error),
            Self::NotASnapshot | Self::Damaged => None,
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Grava `payload` como o snapshot em `path`, substituindo o que houver. Ao voltar `Ok`,
/// ele sobrevive a um crash; antes disso, um crash deixa o snapshot anterior intacto.
///
/// # Errors
/// [`SnapshotError::PayloadTooLarge`] se o payload não cabe no quadro;
/// [`SnapshotError::Io`] se a escrita, o `fsync` ou o `rename` falharem, e nesse caso o
/// snapshot anterior continua no caminho.
pub fn write_snapshot(path: &Path, payload: &[u8]) -> Result<(), SnapshotError> {
    let mut bytes = MAGIC.to_vec();
    encode(payload, &mut bytes).map_err(SnapshotError::PayloadTooLarge)?;
    let temporary = temporary_path(path);
    let mut file = File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent_dir(path)?;
    Ok(())
}

/// O payload do snapshot em `path`, ou `None` se não houver nenhum. Um temporário que um
/// crash deixou para trás não conta: nunca foi renomeado, então nunca foi o snapshot.
///
/// # Errors
/// [`SnapshotError::Io`] se não der para ler; [`SnapshotError::NotASnapshot`] ou
/// [`SnapshotError::Damaged`] se o arquivo não for exatamente um snapshot.
pub fn read_snapshot(path: &Path) -> Result<Option<Vec<u8>>, SnapshotError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let frame = bytes
        .strip_prefix(MAGIC)
        .ok_or(SnapshotError::NotASnapshot)?;
    let decoded = decode(frame);
    match decoded.records.as_slice() {
        [payload] if decoded.valid_len == frame.len() => Ok(Some(payload.to_vec())),
        _ => Err(SnapshotError::Damaged),
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diretório temporário que se apaga no fim do teste, inclusive se ele falhar: o
    /// snapshot e o temporário dele moram lado a lado.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = format!("exchange-snapshot-{}-{name}", std::process::id());
            let path = std::env::temp_dir().join(dir);
            remove_if_present(&path);
            fs::create_dir(&path).expect("criar diretório de teste");
            Self(path)
        }

        fn snapshot(&self) -> PathBuf {
            self.0.join("snapshot")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            remove_if_present(&self.0);
        }
    }

    fn remove_if_present(path: &Path) {
        match fs::remove_dir_all(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                panic!("não deu para limpar {}: {error}", path.display())
            }
            _ => {}
        }
    }

    fn read(path: &Path) -> Option<Vec<u8>> {
        read_snapshot(path).expect("snapshot legível")
    }

    #[test]
    fn missing_snapshot_is_none() {
        let dir = TempDir::new("missing");
        assert_eq!(read(&dir.snapshot()), None);
    }

    #[test]
    fn written_snapshot_reads_back() {
        let dir = TempDir::new("roundtrip");
        write_snapshot(&dir.snapshot(), b"estado").unwrap();
        assert_eq!(read(&dir.snapshot()), Some(b"estado".to_vec()));
    }

    #[test]
    fn empty_payload_is_a_snapshot_too() {
        let dir = TempDir::new("empty");
        write_snapshot(&dir.snapshot(), b"").unwrap();
        assert_eq!(read(&dir.snapshot()), Some(Vec::new()));
    }

    #[test]
    fn new_snapshot_replaces_the_old_and_leaves_no_temporary() {
        let dir = TempDir::new("replace");
        write_snapshot(&dir.snapshot(), b"antigo e mais comprido").unwrap();
        write_snapshot(&dir.snapshot(), b"novo").unwrap();
        assert_eq!(read(&dir.snapshot()), Some(b"novo".to_vec()));
        let names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["snapshot"]);
    }

    // Um log com um registro só tem o mesmo quadro: sem a marca, passaria por snapshot.
    #[test]
    fn a_wal_is_not_a_snapshot() {
        let dir = TempDir::new("wal");
        let mut frame = Vec::new();
        encode(b"registro", &mut frame).unwrap();
        fs::write(dir.snapshot(), &frame).unwrap();
        assert!(matches!(
            read_snapshot(&dir.snapshot()),
            Err(SnapshotError::NotASnapshot)
        ));
    }

    #[test]
    fn a_damaged_frame_is_refused() {
        let dir = TempDir::new("damaged");
        write_snapshot(&dir.snapshot(), b"estado").unwrap();
        let good = fs::read(dir.snapshot()).unwrap();
        let last = good.len() - 1;

        let mut flipped = good.clone();
        flipped[last] ^= 1;
        let mut longer = good.clone();
        longer.push(0);
        for bytes in [&good[..last], &good[..MAGIC.len()], &flipped, &longer] {
            fs::write(dir.snapshot(), bytes).unwrap();
            assert!(matches!(
                read_snapshot(&dir.snapshot()),
                Err(SnapshotError::Damaged)
            ));
        }
    }

    // Os estados em que um crash pode deixar o disco, na ordem dos passos da escrita: o
    // temporário cortado em cada byte (inclusive vazio) e depois completo, ainda sem
    // `rename`; e o `rename` feito. Em cada um, lê-se o snapshot antigo inteiro ou o novo
    // inteiro. O `fsync` em si não se testa assim, porque o cache de páginas sobrevive a
    // qualquer crash de processo; ele fica garantido pela ordem dos passos.
    #[test]
    fn a_crash_at_any_point_leaves_the_old_or_the_new_snapshot() {
        let dir = TempDir::new("crash");
        let path = dir.snapshot();
        let temporary = temporary_path(&path);
        write_snapshot(&path, b"novo").unwrap();
        let new = fs::read(&path).unwrap();

        for previous in [None, Some(b"antigo".as_slice())] {
            match previous {
                Some(old) => write_snapshot(&path, old).unwrap(),
                None => fs::remove_file(&path).unwrap(),
            }
            for cut in 0..=new.len() {
                fs::write(&temporary, &new[..cut]).unwrap();
                assert_eq!(
                    read(&path).as_deref(),
                    previous,
                    "temporário com {cut} bytes"
                );
            }
            fs::rename(&temporary, &path).unwrap();
            assert_eq!(read(&path), Some(b"novo".to_vec()), "depois do rename");
        }
    }

    // O próximo snapshot depois de um crash sobrescreve o temporário que ficou.
    #[test]
    fn a_leftover_temporary_does_not_get_in_the_way() {
        let dir = TempDir::new("leftover");
        fs::write(temporary_path(&dir.snapshot()), b"lixo de um crash").unwrap();
        write_snapshot(&dir.snapshot(), b"estado").unwrap();
        assert_eq!(read(&dir.snapshot()), Some(b"estado".to_vec()));
        assert!(!temporary_path(&dir.snapshot()).exists());
    }
}
