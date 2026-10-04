use std::io::Write;
use std::path::Path;

/// Writes `contents` to `path` atomically by writing to a unique temporary file
/// in the same directory and renaming it over the destination.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(contents)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}
