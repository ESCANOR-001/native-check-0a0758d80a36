//! Read Codex's logical .jsonl path, preferring plain history and falling back
//! only on absence to its compressed sibling. Never rewrite the native profile.
use crate::storage_stage;
use std::{
    fs::Metadata,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub(crate) enum Reader {
    Plain(std::fs::File),
    Compressed(Box<dyn Read>),
}
impl Read for Reader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(file) => file.read(output),
            Self::Compressed(reader) => reader.read(output),
        }
    }
}
impl Reader {
    pub fn skip_exact(&mut self, bytes: u64) -> io::Result<()> {
        if let Self::Plain(file) = self {
            let target = file
                .stream_position()?
                .checked_add(bytes)
                .filter(|end| *end <= file.metadata().map(|m| m.len()).unwrap_or(0))
                .ok_or(io::ErrorKind::UnexpectedEof)?;
            file.seek(SeekFrom::Start(target))?;
        } else if io::copy(&mut self.take(bytes), &mut io::sink())? != bytes {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}
pub(crate) struct Rollout {
    pub reader: Reader,
    pub metadata: Metadata,
    pub compressed: bool,
    pub physical: PathBuf,
    pub identity: storage_stage::Identity,
}
impl Rollout {
    pub fn verify(&self, allow_append: bool) -> io::Result<()> {
        let file = storage_stage::open(&self.physical, false)?;
        let now = file.metadata()?;
        if storage_stage::identity(&file)? != self.identity
            || storage_stage::links(&file)? != 1
            || if allow_append && !self.compressed {
                now.len() < self.metadata.len()
            } else {
                now.len() != self.metadata.len() || now.modified()? != self.metadata.modified()?
            }
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
}

/// The caller must first validate every parent against its configured profile.
pub(crate) fn open(path: &Path, physical_limit: u64) -> io::Result<Rollout> {
    let plain = match path
        .file_name()
        .and_then(|p| p.to_str())
        .and_then(|n| n.strip_suffix(".jsonl.zst"))
    {
        Some(stem) => path.with_file_name(format!("{stem}.jsonl")),
        None => path.to_owned(),
    };
    let (physical, file, compressed) = match storage_stage::open(&plain, false) {
        Ok(file) => (plain, file, false),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && plain.extension().is_some_and(|v| v == "jsonl") =>
        {
            let mut name = plain
                .file_name()
                .ok_or(io::ErrorKind::InvalidInput)?
                .to_os_string();
            name.push(".zst");
            let physical = plain.with_file_name(name);
            let file = storage_stage::open(&physical, false)?;
            (physical, file, true)
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.len() == 0 || metadata.len() > physical_limit || storage_stage::links(&file)? != 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let identity = storage_stage::identity(&file)?;
    let reader = if compressed {
        let mut decoder = zstd::stream::read::Decoder::new(file)?;
        decoder.window_log_max(27)?;
        Reader::Compressed(Box::new(decoder))
    } else {
        Reader::Plain(file)
    };
    Ok(Rollout {
        reader,
        metadata,
        compressed,
        physical,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn plain_seeks_and_compressed_skips_preserve_exact_record_offsets() {
        let root = tempfile::tempdir().unwrap();
        for compressed in [false, true] {
            let path = root.path().join(if compressed {
                "compressed.jsonl"
            } else {
                "plain.jsonl"
            });
            let bytes = b"first\nsecond\nthird\n";
            if compressed {
                fs::write(
                    path.with_extension("jsonl.zst"),
                    zstd::stream::encode_all(bytes.as_slice(), 1).unwrap(),
                )
                .unwrap();
            } else {
                fs::write(&path, bytes).unwrap();
            }
            let mut opened = open(&path, 4096).unwrap();
            opened.reader.skip_exact(6).unwrap();
            let mut selected = [0; 7];
            opened.reader.read_exact(&mut selected).unwrap();
            assert_eq!(&selected, b"second\n");
            opened.reader.skip_exact(6).unwrap();
            assert!(opened.reader.skip_exact(1).is_err());
            opened.verify(false).unwrap();
        }
    }
    #[test]
    fn compressed_history_is_read_in_place_and_plain_takes_precedence() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("saved.jsonl");
        let compressed = path.with_extension("jsonl.zst");
        let bytes = zstd::stream::encode_all(b"saved history\n".as_slice(), 1).unwrap();
        fs::write(&compressed, &bytes).unwrap();
        let mut opened = open(&path, 4096).unwrap();
        let mut read = String::new();
        opened.reader.read_to_string(&mut read).unwrap();
        assert_eq!(read, "saved history\n");
        opened.verify(false).unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(&compressed).unwrap(), bytes);
        fs::write(&path, "plain history\n").unwrap();
        let mut opened = open(&compressed, 4096).unwrap();
        let mut read = String::new();
        opened.reader.read_to_string(&mut read).unwrap();
        assert_eq!(read, "plain history\n");
    }
    #[test]
    fn malformed_linked_and_changed_compressed_files_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("saved.jsonl");
        let compressed = path.with_extension("jsonl.zst");
        fs::write(&compressed, b"corrupt").unwrap();
        assert!(open(&path, 4096)
            .unwrap()
            .reader
            .read_to_end(&mut Vec::new())
            .is_err());
        fs::write(
            &compressed,
            zstd::stream::encode_all(b"safe\n".as_slice(), 1).unwrap(),
        )
        .unwrap();
        let opened = open(&path, 4096).unwrap();
        fs::write(&compressed, b"different").unwrap();
        assert!(opened.verify(true).is_err());
        drop(opened);
        fs::hard_link(&compressed, root.path().join("alias")).unwrap();
        assert!(open(&path, 4096).is_err());
    }
}
