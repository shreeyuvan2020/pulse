//! Where submitted recordings live: plain files under one folder, as
//! `<submission id>/<link>.mp4` and `<link>.timeline.json`. The names are
//! built only from a number and a validated 64-digit hash, so nothing a
//! client sends can point outside the folder. A bucket can replace this
//! later without changing anything above it.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Storage {
    root: PathBuf,
}

/// A hash written as 64 lowercase hex digits, as `pulse_core::chain::to_hex` does.
pub fn is_hex_hash(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

#[derive(Clone, Copy)]
pub enum Kind {
    Video,
    Timeline,
}

impl Storage {
    pub fn new(root: PathBuf) -> Storage {
        Storage { root }
    }

    pub fn path(&self, submission: i64, link: &str, kind: Kind) -> Option<PathBuf> {
        if !is_hex_hash(link) || submission < 0 {
            return None;
        }
        let name = match kind {
            Kind::Video => format!("{link}.mp4"),
            Kind::Timeline => format!("{link}.timeline.json"),
        };
        Some(self.root.join(submission.to_string()).join(name))
    }

    /// Writes the file whole or not at all.
    pub async fn put(
        &self,
        submission: i64,
        link: &str,
        kind: Kind,
        bytes: &[u8],
    ) -> io::Result<()> {
        let path = self
            .path(submission, link, kind)
            .ok_or(io::ErrorKind::InvalidInput)?;
        write(&path, bytes).await
    }

    pub async fn read(&self, submission: i64, link: &str, kind: Kind) -> io::Result<Vec<u8>> {
        let path = self
            .path(submission, link, kind)
            .ok_or(io::ErrorKind::InvalidInput)?;
        tokio::fs::read(path).await
    }

    /// Deletes everything stored for a submission. A missing folder is fine.
    pub async fn remove_submission(&self, submission: i64) -> io::Result<()> {
        match tokio::fs::remove_dir_all(self.root.join(submission.to_string())).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

async fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension("partial");
    tokio::fs::write(&temporary, bytes).await?;
    tokio::fs::rename(temporary, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_hashes_make_paths_and_they_stay_inside_the_folder() {
        let storage = Storage::new(PathBuf::from("/data"));
        let link = "ab".repeat(32);
        assert_eq!(
            storage.path(7, &link, Kind::Video),
            Some(PathBuf::from(format!("/data/7/{link}.mp4")))
        );
        for bad in [
            "../../etc/passwd",
            "",
            &"AB".repeat(32),
            &"ab".repeat(31),
            &format!("{}/", "a".repeat(63)),
        ] {
            assert_eq!(storage.path(7, bad, Kind::Video), None, "{bad}");
        }
        assert_eq!(storage.path(-1, &link, Kind::Timeline), None);
    }
}
