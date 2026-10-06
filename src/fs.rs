use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Writes `contents` to `path` atomically by writing to `<path>.tmp`, syncing it to
/// disk, and renaming it over the destination.
///
/// The temporary path is fixed, so callers must not write the same `path` concurrently.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_added_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(contents)?;
    // Flush data to disk before the rename so a crash can't leave an empty or partial file.
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cache.json");

        write_atomic(&path, b"a much longer first payload").unwrap();
        write_atomic(&path, b"short").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"short");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
}
