//! Filesystem-backed dictation storage.
//!
//! One plain-text file per dictation, named after the UTC timestamp of the
//! recording. The directory is the source of truth: there is no database.

use std::io;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
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
const SNIPPET_BEFORE_CHARS: usize = 60;
const SNIPPET_AFTER_CHARS: usize = 120;

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

/// One run of snippet text, highlighted when it matches the query.
#[derive(Debug, Clone)]
pub struct SnippetPart {
    pub text: String,
    pub highlight: bool,
}

/// A dictation matching a search query, with a snippet around the first match.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: String,
    pub iso: String,
    pub display: String,
    pub snippet: Vec<SnippetPart>,
}

/// Search outcome: the total number of matches and the returned page of hits.
#[derive(Debug, Clone)]
pub struct SearchResults {
    pub total: usize,
    pub hits: Vec<SearchHit>,
}

/// One parsed dictation file, before rendering.
struct StoredEntry {
    at: OffsetDateTime,
    sequence: u32,
    id: String,
    text: String,
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

    /// Read and parse every dictation file, newest first.
    async fn entries(&self) -> Result<Vec<StoredEntry>, StoreError> {
        let mut entries = Vec::new();
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
            let Some((at, sequence)) = parse_id(stem) else {
                continue;
            };
            let text = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "skipping unreadable dictation");
                    continue;
                }
            };
            entries.push(StoredEntry {
                at,
                sequence,
                id: stem.to_string(),
                text,
            });
        }
        entries.sort_by(|a, b| (b.at, b.sequence).cmp(&(a.at, a.sequence)));
        Ok(entries)
    }

    /// All dictations, newest first.
    pub async fn list(&self) -> Result<Vec<DictationSummary>, StoreError> {
        let entries = self.entries().await?;
        let mut summaries = Vec::with_capacity(entries.len());
        for entry in entries {
            let (_, iso, display) = render(entry.at)?;
            summaries.push(DictationSummary {
                id: entry.id,
                iso,
                display,
                preview: preview(&entry.text),
            });
        }
        Ok(summaries)
    }

    /// Find dictations containing every whitespace-separated term, newest first.
    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, StoreError> {
        let patterns: Vec<Regex> = query
            .split_whitespace()
            .map(|term| {
                RegexBuilder::new(&regex::escape(term))
                    .case_insensitive(true)
                    .build()
                    .expect("escaped literal is always a valid pattern")
            })
            .collect();
        if patterns.is_empty() {
            return Ok(SearchResults {
                total: 0,
                hits: Vec::new(),
            });
        }

        let mut total = 0;
        let mut hits = Vec::new();
        for entry in self.entries().await? {
            if !patterns.iter().all(|pattern| pattern.is_match(&entry.text)) {
                continue;
            }
            total += 1;
            if hits.len() < limit {
                let (_, iso, display) = render(entry.at)?;
                hits.push(SearchHit {
                    id: entry.id,
                    iso,
                    display,
                    snippet: snippet(&entry.text, &patterns),
                });
            }
        }
        Ok(SearchResults { total, hits })
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

/// Window of text around the earliest match, with every query term highlighted.
fn snippet(text: &str, patterns: &[Regex]) -> Vec<SnippetPart> {
    let first = patterns
        .iter()
        .filter_map(|pattern| pattern.find(text))
        .min_by_key(|found| found.start())
        .expect("caller matched at least one pattern");
    let start = back_chars(text, first.start(), SNIPPET_BEFORE_CHARS);
    let end = forward_chars(text, first.end(), SNIPPET_AFTER_CHARS);
    let window = &text[start..end];

    let mut ranges = Vec::new();
    for pattern in patterns {
        for found in pattern.find_iter(window) {
            ranges.push((start + found.start(), start + found.end()));
        }
    }
    ranges.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }

    let mut parts = Vec::new();
    if start > 0 {
        parts.push(SnippetPart {
            text: "… ".to_string(),
            highlight: false,
        });
    }
    let mut cursor = start;
    for (s, e) in merged {
        if s > cursor {
            parts.push(plain(&text[cursor..s]));
        }
        parts.push(SnippetPart {
            text: flat(&text[s..e]),
            highlight: true,
        });
        cursor = e;
    }
    if cursor < end {
        parts.push(plain(&text[cursor..end]));
    }
    if end < text.len() {
        parts.push(SnippetPart {
            text: " …".to_string(),
            highlight: false,
        });
    }

    if let Some(first) = parts.first_mut() {
        if !first.highlight {
            first.text = first.text.trim_start().to_string();
        }
    }
    if let Some(last) = parts.last_mut() {
        if !last.highlight {
            last.text = last.text.trim_end().to_string();
        }
    }
    parts.retain(|part| part.highlight || !part.text.is_empty());
    parts
}

fn plain(text: &str) -> SnippetPart {
    SnippetPart {
        text: flat(text),
        highlight: false,
    }
}

/// Replace each whitespace character with one space; keeps byte offsets
/// stable while making the one-line snippet readable.
fn flat(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}

/// Byte offset of the character `count` characters before `byte`, or 0.
fn back_chars(text: &str, byte: usize, count: usize) -> usize {
    text[..byte]
        .char_indices()
        .rev()
        .take(count)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Byte offset `count` characters after `byte`, or `text.len()`.
fn forward_chars(text: &str, byte: usize, count: usize) -> usize {
    text[byte..]
        .char_indices()
        .nth(count)
        .map(|(i, _)| byte + i)
        .unwrap_or(text.len())
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

    async fn search_terms(store: &DictationStore, query: &str) -> SearchResults {
        store.search(query, 100).await.unwrap()
    }

    #[tokio::test]
    async fn search_matches_all_terms_case_insensitively() {
        let (_guard, store) = store().await;
        let older = store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "Alpha beta gamma")
            .await
            .unwrap();
        let newer = store
            .save_at(datetime!(2026-10-21 09:15:00 UTC), "alpha only")
            .await
            .unwrap();

        let results = search_terms(&store, "ALPHA").await;
        assert_eq!(results.total, 2);
        assert_eq!(results.hits[0].id, newer.id);
        assert_eq!(results.hits[1].id, older.id);

        let results = search_terms(&store, "alpha beta").await;
        assert_eq!(results.total, 1);
        assert_eq!(results.hits[0].id, older.id);
    }

    #[tokio::test]
    async fn search_is_unicode_case_insensitive() {
        let (_guard, store) = store().await;
        store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "L'école est fermée")
            .await
            .unwrap();
        assert_eq!(search_terms(&store, "ÉCOLE").await.total, 1);
    }

    #[tokio::test]
    async fn search_treats_query_as_literal() {
        let (_guard, store) = store().await;
        store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "a.b")
            .await
            .unwrap();
        assert_eq!(search_terms(&store, "a.b").await.total, 1);
        assert_eq!(search_terms(&store, "axb").await.total, 0);
        assert_eq!(search_terms(&store, "a.*b").await.total, 0);
    }

    #[tokio::test]
    async fn search_reports_highlighted_snippet() {
        let (_guard, short_store) = store().await;
        short_store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "alpha beta gamma")
            .await
            .unwrap();
        let results = search_terms(&short_store, "beta").await;
        let snippet = &results.hits[0].snippet;
        let joined: String = snippet.iter().map(|part| part.text.as_str()).collect();
        assert_eq!(joined, "alpha beta gamma");
        let highlighted: Vec<&str> = snippet
            .iter()
            .filter(|part| part.highlight)
            .map(|part| part.text.as_str())
            .collect();
        assert_eq!(highlighted, ["beta"]);

        let (_long_guard, long_store) = store().await;
        let long = format!("{}needle{}", "a".repeat(200), "b".repeat(200));
        long_store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), &long)
            .await
            .unwrap();
        let results = search_terms(&long_store, "needle").await;
        let snippet = &results.hits[0].snippet;
        assert_eq!(snippet.first().unwrap().text, "… ");
        assert!(snippet.last().unwrap().text.ends_with('…'));

        let (_multiline_guard, multiline_store) = store().await;
        multiline_store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "alpha\nbeta gamma")
            .await
            .unwrap();
        let results = search_terms(&multiline_store, "beta").await;
        assert!(results.hits[0]
            .snippet
            .iter()
            .all(|part| !part.text.contains('\n')));
    }

    #[tokio::test]
    async fn search_limits_hits_but_counts_all() {
        let (_guard, store) = store().await;
        for (at, text) in [
            (datetime!(2026-10-20 12:00:00 UTC), "match one"),
            (datetime!(2026-10-20 13:00:00 UTC), "match two"),
            (datetime!(2026-10-20 14:00:00 UTC), "match three"),
        ] {
            store.save_at(at, text).await.unwrap();
        }
        let results = store.search("match", 2).await.unwrap();
        assert_eq!(results.total, 3);
        assert_eq!(results.hits.len(), 2);
    }

    #[tokio::test]
    async fn search_with_no_terms_is_empty() {
        let (_guard, store) = store().await;
        store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "anything")
            .await
            .unwrap();
        let results = store.search("   ", 100).await.unwrap();
        assert_eq!(results.total, 0);
        assert!(results.hits.is_empty());
    }
}
