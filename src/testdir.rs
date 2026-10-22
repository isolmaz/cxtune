//! Scratch directories for tests: created under the OS temp directory and removed when the value
//! goes out of scope, whether the test passed or not.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cxtune-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_is_removed_with_a_read_only_file_in_it_and_after_a_failed_test() {
        let kept = {
            let dir = TempDir::new("testdir");
            let file = dir.join("held.toml");
            std::fs::write(&file, "x").unwrap();
            let mut permissions = std::fs::metadata(&file).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&file, permissions).unwrap();
            dir.to_path_buf()
        };
        assert!(!kept.exists());

        let outcome = std::panic::catch_unwind(|| {
            let dir = TempDir::new("testdir");
            std::fs::write(dir.join("a"), "x").unwrap();
            panic!("{}", dir.display());
        });
        let message = *outcome.unwrap_err().downcast::<String>().unwrap();
        assert!(!Path::new(&message).exists());
    }
}
