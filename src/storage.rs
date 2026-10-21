//! Filesystem-backed dictation storage.
//!
//! One plain-text file per dictation, named after the UTC timestamp of the
//! recording. The directory is the source of truth: there is no database.

use std::io;
use std::path::{Path, PathBuf};

use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

/// Filename timestamp, e.g. `2026-10-20T12-34-56.123456Z`.
pub const FILE_FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]-[minute]-[second].[subsecond digits:6]Z");
/// Machine-readable timestamp used in `<time datetime="...">`.
pub const ISO_FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
/// Human-readable UTC timestamp for no-JS rendering.
pub const DISPLAY_FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[day] [month repr:short] [year], [hour]:[minute] UTC");

const MAX_SEQUENCE: u32 = 1000;
const PREVIEW_CHARS: usize = 140;

/// A dictation as shown in the index list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DictationSummary {
    pub id: String,
    pub iso: String,
    pub display: String,
    pub preview: String,
}

/// A single stored dictation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Dictation {
    pub id: String,
    pub iso: String,
    pub display: String,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Io(#[from] io::Error),
    #[error("no speech was detected")]
    Empty,
    #[error("timestamp formatting error: {0}")]
    Format(#[from] time::error::Format),
    #[error("too many dictations share this timestamp")]
    Exhausted,
}

/// Directory of `*.txt` dictations.
pub struct DictationStore {
    dir: PathBuf,
}

impl DictationStore {
    pub async fn new(dir: PathBuf) -> Result<Self, StoreError> {
        tokio::fs::create_dir_all(&dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        Ok(Self { dir })
    }

    /// Save `text` under the current UTC time.
    pub async fn save(&self, text: &str) -> Result<Dictation, StoreError> {
        self.save_at(OffsetDateTime::now_utc(), text).await
    }

    /// Save `text` under an explicit timestamp; used by tests.
    pub(crate) async fn save_at(
        &self,
        at: OffsetDateTime,
        text: &str,
    ) -> Result<Dictation, StoreError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(StoreError::Empty);
        }
        let (base, iso, display) = render(at)?;
        let body = format!("{}\n", text.trim_end());

        for sequence in 0..MAX_SEQUENCE {
            let id = if sequence == 0 {
                base.clone()
            } else {
                format!("{base}-{sequence}")
            };
            let path = self.dir.join(format!("{id}.txt"));
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = match options.open(&path).await {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            };
            if let Err(err) = tokio::io::AsyncWriteExt::write_all(&mut file, body.as_bytes()).await {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(err.into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
            }
            return Ok(Dictation {
                id,
                iso,
                display,
                text: text.to_string(),
            });
        }
        Err(StoreError::Exhausted)
    }

    /// All dictations, newest first.
    pub async fn list(&self) -> Result<Vec<DictationSummary>, StoreError> {
        let mut entries: Vec<((OffsetDateTime, u32), DictationSummary)> = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if !entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if path.extension() != Some(std::ffi::OsStr::new("txt")) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(key) = parse_id(stem) else {
                continue;
            };
            let text = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "skipping unreadable dictation");
                    continue;
                }
            };
            let (_, iso, display) = render(key.0)?;
            entries.push((
                key,
                DictationSummary {
                    id: stem.to_string(),
                    iso,
                    display,
                    preview: preview(&text),
                },
            ));
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(entries.into_iter().map(|(_, summary)| summary).collect())
    }

    /// Load one dictation; invalid or unknown ids are `None`.
    pub async fn get(&self, id: &str) -> Result<Option<Dictation>, StoreError> {
        let Some((at, _sequence)) = parse_id(id) else {
            return Ok(None);
        };
        let path = self.dir.join(format!("{id}.txt"));
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let (_, iso, display) = render(at)?;
        Ok(Some(Dictation {
            id: id.to_string(),
            iso,
            display,
            text: text.trim_end().to_string(),
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Format the same instant as a filename stem, ISO timestamp and display string.
fn render(at: OffsetDateTime) -> Result<(String, String, String), StoreError> {
    let at = at.to_offset(UtcOffset::UTC);
    let primitive = PrimitiveDateTime::new(at.date(), at.time());
    Ok((
        primitive.format(FILE_FORMAT)?,
        primitive.format(ISO_FORMAT)?,
        primitive.format(DISPLAY_FORMAT)?,
    ))
}

/// Parse a filename stem (with optional `-<sequence>` suffix) back to UTC time.
fn parse_id(id: &str) -> Option<(OffsetDateTime, u32)> {
    let (base, sequence) = match id.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
            (head, tail.parse::<u32>().ok()?)
        }
        _ => (id, 0),
    };
    let primitive = PrimitiveDateTime::parse(base, FILE_FORMAT).ok()?;
    Some((primitive.assume_utc(), sequence))
}

/// Collapse whitespace and shorten to a one-line preview.
fn preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "(empty)".to_string();
    }
    if collapsed.chars().count() > PREVIEW_CHARS {
        let mut truncated: String = collapsed.chars().take(PREVIEW_CHARS).collect();
        truncated.push('…');
        truncated
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    async fn store() -> (tempfile::TempDir, DictationStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = DictationStore::new(dir.path().join("dictations")).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn saves_timestamp_filename_and_newline() {
        let (_guard, store) = store().await;
        let at = datetime!(2026-10-20 12:34:56.123456 UTC);
        let saved = store.save_at(at, "hello world").await.unwrap();
        assert_eq!(saved.id, "2026-10-20T12-34-56.123456Z");
        assert_eq!(saved.iso, "2026-10-20T12:34:56Z");
        assert_eq!(saved.display, "20 Oct 2026, 12:34 UTC");
        let content = tokio::fs::read_to_string(store.dir().join(format!("{}.txt", saved.id)))
            .await
            .unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn same_timestamp_gets_sequence_suffix() {
        let (_guard, store) = store().await;
        let at = datetime!(2026-10-20 12:34:56.123456 UTC);
        let first = store.save_at(at, "first").await.unwrap();
        let second = store.save_at(at, "second").await.unwrap();
        assert_eq!(first.id, "2026-10-20T12-34-56.123456Z");
        assert_eq!(second.id, "2026-10-20T12-34-56.123456Z-1");
        assert!(store.dir().join(format!("{}.txt", first.id)).is_file());
        assert!(store.dir().join(format!("{}.txt", second.id)).is_file());
    }

    #[tokio::test]
    async fn lists_newest_first_and_ignores_other_entries() {
        let (_guard, store) = store().await;
        store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "older")
            .await
            .unwrap();
        store
            .save_at(datetime!(2026-10-21 09:15:00 UTC), "newer")
            .await
            .unwrap();
        tokio::fs::write(store.dir().join("notes.md"), "ignore me")
            .await
            .unwrap();
        tokio::fs::create_dir(store.dir().join("subdir")).await.unwrap();
        tokio::fs::create_dir(store.dir().join("2026-01-01T00-00-00.000000Z.txt"))
            .await
            .unwrap();
        tokio::fs::write(
            store.dir().join("1999-01-01T00-00-00.000000Z.txt"),
            "directly in the directory",
        )
        .await
        .unwrap();
        let list = store.list().await.unwrap();
        let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "2026-10-21T09-15-00.000000Z",
                "2026-10-20T12-00-00.000000Z",
                "1999-01-01T00-00-00.000000Z"
            ]
        );
        assert_eq!(list[0].preview, "newer");
    }

    #[tokio::test]
    async fn rejects_invalid_ids() {
        let (_guard, store) = store().await;
        assert!(store.get("../x").await.unwrap().is_none());
        assert!(store.get("not-an-id").await.unwrap().is_none());
        assert!(store.get("2026-10-20T12-34-56.123456Z").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_returns_saved_text() {
        let (_guard, store) = store().await;
        let saved = store
            .save_at(datetime!(2026-10-20 12:34:56 UTC), "  keep me  ")
            .await
            .unwrap();
        let loaded = store.get(&saved.id).await.unwrap().unwrap();
        assert_eq!(loaded.text, "keep me");
        assert_eq!(loaded.iso, saved.iso);
        assert_eq!(loaded.display, saved.display);
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let (_guard, store) = store().await;
        assert!(matches!(store.save("   \n").await, Err(StoreError::Empty)));
    }

    #[test]
    fn preview_collapses_and_truncates() {
        assert_eq!(preview("a  b\n c "), "a b c");
        assert_eq!(preview(" \n\t "), "(empty)");
        let long = "x".repeat(200);
        let shortened = preview(&long);
        assert_eq!(shortened.chars().count(), PREVIEW_CHARS + 1);
        assert!(shortened.ends_with('…'));
        let exact = "y".repeat(PREVIEW_CHARS);
        assert_eq!(preview(&exact), exact);
        let multibyte = "é".repeat(200);
        assert_eq!(preview(&multibyte).chars().count(), PREVIEW_CHARS + 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn files_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_guard, store) = store().await;
        let saved = store.save("secret").await.unwrap();
        let meta = std::fs::metadata(store.dir().join(format!("{}.txt", saved.id))).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let dir_meta = std::fs::metadata(store.dir()).unwrap();
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);
    }
}
