//! Batches kept as files: one `<batch id>.jsonl` per batch in a directory.
//! Writers on different branches never touch the same file, so merging two
//! histories is merging two directories.

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use uuid::Uuid;

use super::Batch;
use crate::{Error, Result};

/// A directory of batch files.
#[derive(Clone, Debug)]
pub struct BatchDir {
    dir: PathBuf,
}

impl BatchDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        BatchDir { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, batch: &Uuid) -> PathBuf {
        self.dir.join(format!("{batch}.jsonl"))
    }

    pub fn contains(&self, batch: &Uuid) -> bool {
        self.path(batch).exists()
    }

    /// Every batch in the directory (in no particular order; see [`super::order`]).
    pub fn read<O: Serialize + DeserializeOwned>(&self) -> Result<Vec<Batch<O>>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let text = fs::read_to_string(&path)?;
                out.push(
                    Batch::from_jsonl(&text)
                        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?,
                );
            }
        }
        Ok(out)
    }

    /// Writes a batch (atomically). A batch already there is left as it is.
    pub fn write<O: Serialize + DeserializeOwned>(&self, batch: &Batch<O>) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(&batch.header.batch);
        if !path.exists() {
            write_atomic(&path, batch.to_jsonl().as_bytes())?;
        }
        Ok(path)
    }
}

/// Writes through a temporary file and a rename, so readers never see half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::batch;
    use super::*;

    #[test]
    fn writes_and_reads_batches() {
        let dir = tempfile::tempdir().unwrap();
        let log = BatchDir::new(dir.path().join("log"));
        let b = batch(1, None, vec![3]);
        log.write(&b).unwrap();
        log.write(&b).unwrap();
        assert!(log.contains(&b.header.batch));
        assert_eq!(log.read::<i64>().unwrap(), vec![b]);
        std::fs::write(dir.path().join("log").join("broken.jsonl"), "not json").unwrap();
        assert!(log
            .read::<i64>()
            .unwrap_err()
            .to_string()
            .contains("broken.jsonl"));
    }
}
