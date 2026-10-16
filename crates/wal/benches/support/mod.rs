use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Do tamanho de um `LogRecord` de submissão em postcard; o conteúdo não importa.
pub const PAYLOAD: [u8; 21] = [0xAB; 21];

/// Um log novo dentro de `target/tmp`, no disco de verdade. Em `/tmp`, que pode ser
/// `tmpfs` (RAM), o `fsync` não faz nada e o benchmark mediria uma durabilidade falsa.
pub struct BenchLog(PathBuf);

impl BenchLog {
    pub fn new(name: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("bench-{name}.wal"));
        remove_if_present(&path);
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for BenchLog {
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
