use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::sequencer::snapshot_path;

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

/// O log leva junto o snapshot dele e o temporário que um crash pode deixar.
impl Drop for TempPath {
    fn drop(&mut self) {
        let snapshot = snapshot_path(&self.0);
        let mut temporary = snapshot.clone().into_os_string();
        temporary.push(".tmp");
        for path in [&self.0, &snapshot, Path::new(&temporary)] {
            remove_if_present(path);
        }
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
