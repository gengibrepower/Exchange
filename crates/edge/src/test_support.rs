use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Caminho temporário que se apaga no fim do teste, inclusive se ele falhar.
pub(crate) struct TempPath(pub(crate) PathBuf);

impl TempPath {
    pub(crate) fn new(name: &str) -> Self {
        let file = format!("exchange-edge-lib-{}-{name}", std::process::id());
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
