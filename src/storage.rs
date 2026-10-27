//! SQLite-backed dictation storage.
//!
//! Notes live in the `notes` table of the shared [`Database`]. Ids keep the
//! timestamp form used in URLs and are unique; notes sharing a timestamp get
//! a `-<sequence>` suffix.

use regex::{Regex, RegexBuilder};
use rusqlite::{params, params_from_iter, OptionalExtension};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use crate::db::Database;

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

/// An in-progress note that has not been saved to the notes list yet.
#[derive(Debug, Clone)]
pub struct Draft {
    pub id: i64,
    pub name: String,
    pub text: String,
    /// Microseconds since the epoch; the draft's note timestamp when saved.
    pub created_at: i64,
    pub created_iso: String,
    pub created_display: String,
    pub updated_iso: String,
    pub updated_display: String,
}

impl Draft {
    /// Human-readable label for the draft list.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            "Untitled draft"
        } else {
            &self.name
        }
    }
}

/// A draft as shown in the draft list.
#[derive(Debug, Clone)]
pub struct DraftSummary {
    pub id: i64,
    pub name: String,
    pub preview: String,
    pub updated_iso: String,
    pub updated_display: String,
}

impl DraftSummary {
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            "Untitled draft"
        } else {
            &self.name
        }
    }
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

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] tokio_rusqlite::Error),
    #[error("no speech was detected")]
    Empty,
    #[error("timestamp formatting error: {0}")]
    Format(#[from] time::error::Format),
    #[error("invalid timestamp: {0}")]
    Timestamp(#[from] time::error::ComponentRange),
    #[error("too many dictations share this timestamp")]
    Exhausted,
    #[error("draft not found")]
    NotFound,
}

/// SQLite-backed store for all notes.
pub struct DictationStore {
    db: Database,
}

impl DictationStore {
    pub fn new(db: Database) -> Self {
        Self { db }
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
        let recorded_at = to_micros(at);
        let body = text.to_string();
        let body_lower = body.to_lowercase();

        for sequence in 0..MAX_SEQUENCE {
            let id = if sequence == 0 {
                base.clone()
            } else {
                format!("{base}-{sequence}")
            };
            let insert_id = id.clone();
            let insert_body = body.clone();
            let insert_lower = body_lower.clone();
            let inserted = self
                .db
                .call(move |conn| {
                    insert_note(
                        conn,
                        &insert_id,
                        recorded_at,
                        sequence,
                        &insert_body,
                        &insert_lower,
                    )
                })
                .await?;
            if inserted {
                return Ok(Dictation {
                    id,
                    iso,
                    display,
                    text: body,
                });
            }
        }
        Err(StoreError::Exhausted)
    }

    /// All dictations, newest first.
    pub async fn list(&self) -> Result<Vec<DictationSummary>, StoreError> {
        let rows = self
            .db
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, recorded_at, text FROM notes
                     ORDER BY recorded_at DESC, sequence DESC",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for (id, recorded_at, text) in rows {
            let at = from_micros(recorded_at)?;
            let (_, iso, display) = render(at)?;
            summaries.push(DictationSummary {
                id,
                iso,
                display,
                preview: preview(&text),
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

        // ASCII terms can be filtered in SQL against the pre-folded column.
        // Terms containing non-ASCII characters are verified in Rust so that
        // SQLite's simpler case mapping cannot hide a legitimate match.
        let terms: Vec<String> = query
            .split_whitespace()
            .filter(|term| term.is_ascii())
            .map(|term| term.to_ascii_lowercase())
            .collect();

        let rows = self
            .db
            .call(move |conn| {
                let mut sql = String::from("SELECT id, recorded_at, text FROM notes WHERE 1 = 1");
                for _ in &terms {
                    sql.push_str(" AND instr(text_lower, ?) > 0");
                }
                sql.push_str(" ORDER BY recorded_at DESC, sequence DESC");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(params_from_iter(terms.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut total = 0;
        let mut hits = Vec::new();
        for (id, recorded_at, text) in rows {
            if !patterns.iter().all(|pattern| pattern.is_match(&text)) {
                continue;
            }
            total += 1;
            if hits.len() < limit {
                let at = from_micros(recorded_at)?;
                let (_, iso, display) = render(at)?;
                hits.push(SearchHit {
                    id,
                    iso,
                    display,
                    snippet: snippet(&text, &patterns),
                });
            }
        }
        Ok(SearchResults { total, hits })
    }

    /// Load one dictation; unknown ids are `None`.
    pub async fn get(&self, id: &str) -> Result<Option<Dictation>, StoreError> {
        let lookup = id.to_string();
        let row = self
            .db
            .call(move |conn| {
                conn.query_row(
                    "SELECT recorded_at, text FROM notes WHERE id = ?1",
                    params![lookup],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
            })
            .await?;
        let Some((recorded_at, text)) = row else {
            return Ok(None);
        };
        let at = from_micros(recorded_at)?;
        let (_, iso, display) = render(at)?;
        Ok(Some(Dictation {
            id: id.to_string(),
            iso,
            display,
            text,
        }))
    }

    /// Create an empty draft and return its id.
    pub async fn create_draft(&self) -> Result<i64, StoreError> {
        self.create_draft_at(OffsetDateTime::now_utc()).await
    }

    /// Create an empty draft with an explicit start time; used by tests.
    pub(crate) async fn create_draft_at(&self, at: OffsetDateTime) -> Result<i64, StoreError> {
        let created_at = to_micros(at);
        let id = self
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO drafts (name, created_at, updated_at, text)
                     VALUES ('', ?1, ?1, '')",
                    params![created_at],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        Ok(id)
    }

    /// All drafts, most recently edited first.
    pub async fn list_drafts(&self) -> Result<Vec<DraftSummary>, StoreError> {
        let rows = self
            .db
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, updated_at, substr(text, 1, 400) FROM drafts
                     ORDER BY updated_at DESC, id DESC",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut drafts = Vec::with_capacity(rows.len());
        for (id, name, updated_at, text) in rows {
            let updated = from_micros(updated_at)?;
            let (_, updated_iso, updated_display) = render(updated)?;
            drafts.push(DraftSummary {
                id,
                name,
                preview: preview(&text),
                updated_iso,
                updated_display,
            });
        }
        Ok(drafts)
    }

    /// Load one draft; unknown ids are `None`.
    pub async fn get_draft(&self, id: i64) -> Result<Option<Draft>, StoreError> {
        let row = self
            .db
            .call(move |conn| {
                conn.query_row(
                    "SELECT name, created_at, updated_at, text FROM drafts WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()
            })
            .await?;
        let Some((name, created_at, updated_at, text)) = row else {
            return Ok(None);
        };
        render_draft(id, name, created_at, updated_at, text).map(Some)
    }

    /// Replace the supplied draft fields and bump its edit time.
    pub async fn update_draft(
        &self,
        id: i64,
        name: Option<String>,
        text: Option<String>,
    ) -> Result<Option<Draft>, StoreError> {
        let name = name.map(|name| name.trim().to_string());
        let text = text.map(|text| text.trim().to_string());
        let updated_at = to_micros(OffsetDateTime::now_utc());
        let changed = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE drafts
                     SET name = COALESCE(?2, name),
                         text = COALESCE(?3, text),
                         updated_at = ?4
                     WHERE id = ?1",
                    params![id, name, text, updated_at],
                )
            })
            .await?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_draft(id).await
    }

    /// Delete a draft; false when it was already gone.
    pub async fn delete_draft(&self, id: i64) -> Result<bool, StoreError> {
        let deleted = self
            .db
            .call(move |conn| conn.execute("DELETE FROM drafts WHERE id = ?1", params![id]))
            .await?;
        Ok(deleted > 0)
    }

    /// Move a draft into the notes table and delete it.
    ///
    /// `text` overrides the stored draft body so a save still succeeds when the
    /// last background autosave failed. The note is dated from the draft's
    /// creation, not from the moment Save was pressed.
    pub async fn save_draft(&self, id: i64, text: Option<&str>) -> Result<Dictation, StoreError> {
        let Some(draft) = self.get_draft(id).await? else {
            return Err(StoreError::NotFound);
        };
        let body = match text {
            Some(text) => text.trim().to_string(),
            None => draft.text.trim().to_string(),
        };
        if body.is_empty() {
            return Err(StoreError::Empty);
        }
        let at = from_micros(draft.created_at)?;
        let (base, iso, display) = render(at)?;
        let recorded_at = draft.created_at;
        let body_lower = body.to_lowercase();
        let base_for_call = base.clone();
        let body_for_call = body.clone();

        let outcome = self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM drafts WHERE id = ?1)",
                    params![id],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Ok(PromoteOutcome::Missing);
                }
                for sequence in 0..MAX_SEQUENCE {
                    let note_id = if sequence == 0 {
                        base_for_call.clone()
                    } else {
                        format!("{base_for_call}-{sequence}")
                    };
                    if insert_note(
                        &tx,
                        &note_id,
                        recorded_at,
                        sequence,
                        &body_for_call,
                        &body_lower,
                    )? {
                        tx.execute("DELETE FROM drafts WHERE id = ?1", params![id])?;
                        tx.commit()?;
                        return Ok(PromoteOutcome::Saved(note_id));
                    }
                }
                Ok(PromoteOutcome::Exhausted)
            })
            .await?;

        match outcome {
            PromoteOutcome::Saved(note_id) => Ok(Dictation {
                id: note_id,
                iso,
                display,
                text: body,
            }),
            PromoteOutcome::Missing => Err(StoreError::NotFound),
            PromoteOutcome::Exhausted => Err(StoreError::Exhausted),
        }
    }
}

enum PromoteOutcome {
    Saved(String),
    Missing,
    Exhausted,
}

fn insert_note(
    conn: &rusqlite::Connection,
    id: &str,
    recorded_at: i64,
    sequence: u32,
    body: &str,
    body_lower: &str,
) -> rusqlite::Result<bool> {
    let result = conn.execute(
        "INSERT INTO notes (id, recorded_at, sequence, text, text_lower)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, recorded_at, sequence, body, body_lower],
    );
    match result {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn render_draft(
    id: i64,
    name: String,
    created_at: i64,
    updated_at: i64,
    text: String,
) -> Result<Draft, StoreError> {
    let created = from_micros(created_at)?;
    let updated = from_micros(updated_at)?;
    let (_, created_iso, created_display) = render(created)?;
    let (_, updated_iso, updated_display) = render(updated)?;
    Ok(Draft {
        id,
        name,
        text,
        created_at,
        created_iso,
        created_display,
        updated_iso,
        updated_display,
    })
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

fn to_micros(at: OffsetDateTime) -> i64 {
    (at.unix_timestamp_nanos() / 1000) as i64
}

fn from_micros(micros: i64) -> Result<OffsetDateTime, StoreError> {
    Ok(OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(micros) * 1000,
    )?)
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
    use crate::db::Database;
    use time::macros::datetime;

    async fn store() -> (tempfile::TempDir, DictationStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).await.unwrap();
        (dir, DictationStore::new(db))
    }

    #[tokio::test]
    async fn saves_timestamp_id_and_text() {
        let (_guard, store) = store().await;
        let at = datetime!(2026-10-20 12:34:56.123456 UTC);
        let saved = store.save_at(at, "hello world").await.unwrap();
        assert_eq!(saved.id, "2026-10-20T12-34-56.123456Z");
        assert_eq!(saved.iso, "2026-10-20T12:34:56Z");
        assert_eq!(saved.display, "20 Oct 2026, 12:34 UTC");
        let loaded = store.get(&saved.id).await.unwrap().unwrap();
        assert_eq!(loaded.text, "hello world");
    }

    #[tokio::test]
    async fn same_timestamp_gets_sequence_suffix() {
        let (_guard, store) = store().await;
        let at = datetime!(2026-10-20 12:34:56.123456 UTC);
        let first = store.save_at(at, "first").await.unwrap();
        let second = store.save_at(at, "second").await.unwrap();
        assert_eq!(first.id, "2026-10-20T12-34-56.123456Z");
        assert_eq!(second.id, "2026-10-20T12-34-56.123456Z-1");
        assert_eq!(store.get(&first.id).await.unwrap().unwrap().text, "first");
        assert_eq!(store.get(&second.id).await.unwrap().unwrap().text, "second");
    }

    #[tokio::test]
    async fn lists_newest_first() {
        let (_guard, store) = store().await;
        store
            .save_at(datetime!(2026-10-20 12:00:00 UTC), "older")
            .await
            .unwrap();
        store
            .save_at(datetime!(2026-10-21 09:15:00 UTC), "newer")
            .await
            .unwrap();
        let list = store.list().await.unwrap();
        let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            ["2026-10-21T09-15-00.000000Z", "2026-10-20T12-00-00.000000Z"]
        );
        assert_eq!(list[0].preview, "newer");
    }

    #[tokio::test]
    async fn rejects_invalid_ids() {
        let (_guard, store) = store().await;
        assert!(store.get("../x").await.unwrap().is_none());
        assert!(store.get("not-an-id").await.unwrap().is_none());
        assert!(store
            .get("2026-10-20T12-34-56.123456Z")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn gets_saved_text_trimmed() {
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

    #[tokio::test]
    async fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let db = Database::open(dir.path()).await.unwrap();
            let store = DictationStore::new(db);
            store.save("durable").await.unwrap().id
        };
        let db = Database::open(dir.path()).await.unwrap();
        let store = DictationStore::new(db);
        assert_eq!(store.get(&id).await.unwrap().unwrap().text, "durable");
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

    #[tokio::test]
    async fn drafts_are_named_updated_and_deleted() {
        let (_guard, store) = store().await;
        let first = store
            .create_draft_at(datetime!(2026-10-20 12:00:00 UTC))
            .await
            .unwrap();
        let second = store
            .create_draft_at(datetime!(2026-10-20 12:01:00 UTC))
            .await
            .unwrap();

        let updated = store
            .update_draft(
                first,
                Some("  Shopping  ".to_string()),
                Some("  milk  ".to_string()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "Shopping");
        assert_eq!(updated.text, "milk");
        assert_eq!(updated.created_iso, "2026-10-20T12:00:00Z");

        let list = store.list_drafts().await.unwrap();
        assert_eq!(list.len(), 2);
        let first_summary = list.iter().find(|draft| draft.id == first).unwrap();
        assert_eq!(first_summary.label(), "Shopping");
        assert_eq!(first_summary.preview, "milk");

        assert!(store.delete_draft(first).await.unwrap());
        assert!(!store.delete_draft(first).await.unwrap());
        assert!(store.get_draft(first).await.unwrap().is_none());
        assert!(store.get_draft(second).await.unwrap().is_some());
        assert!(store
            .update_draft(first, Some("x".into()), None)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn saving_a_draft_uses_its_start_time_and_removes_it() {
        let (_guard, store) = store().await;
        let at = datetime!(2026-10-20 12:34:56.123456 UTC);
        let id = store.create_draft_at(at).await.unwrap();
        store
            .update_draft(
                id,
                Some("Note".to_string()),
                Some("hello draft".to_string()),
            )
            .await
            .unwrap();

        let saved = store.save_draft(id, None).await.unwrap();
        assert_eq!(saved.id, "2026-10-20T12-34-56.123456Z");
        assert_eq!(saved.iso, "2026-10-20T12:34:56Z");
        assert_eq!(saved.text, "hello draft");
        assert!(store.get_draft(id).await.unwrap().is_none());
        assert_eq!(store.list().await.unwrap().len(), 1);
        assert_eq!(store.list_drafts().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn saving_a_draft_accepts_the_clients_latest_text() {
        let (_guard, store) = store().await;
        let id = store
            .create_draft_at(datetime!(2026-10-20 12:00:00 UTC))
            .await
            .unwrap();
        store
            .update_draft(id, None, Some("old autosave".to_string()))
            .await
            .unwrap();

        let saved = store.save_draft(id, Some("newer text")).await.unwrap();
        assert_eq!(saved.text, "newer text");
        assert!(store.get_draft(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_draft_is_not_saved() {
        let (_guard, store) = store().await;
        let id = store
            .create_draft_at(datetime!(2026-10-20 12:00:00 UTC))
            .await
            .unwrap();
        assert!(matches!(
            store.save_draft(id, None).await,
            Err(StoreError::Empty)
        ));
        assert!(matches!(
            store.save_draft(id, Some("   ")).await,
            Err(StoreError::Empty)
        ));
        assert!(store.get_draft(id).await.unwrap().is_some());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn saved_draft_shares_timestamp_with_existing_note() {
        let (_guard, store) = store().await;
        let at = datetime!(2026-10-20 12:00:00 UTC);
        store.save_at(at, "existing").await.unwrap();
        let id = store.create_draft_at(at).await.unwrap();
        store
            .update_draft(id, None, Some("draft".to_string()))
            .await
            .unwrap();
        let saved = store.save_draft(id, None).await.unwrap();
        assert_eq!(saved.id, "2026-10-20T12-00-00.000000Z-1");
    }

    #[tokio::test]
    async fn unknown_draft_operations_fail() {
        let (_guard, store) = store().await;
        assert!(matches!(
            store.save_draft(99, Some("text")).await,
            Err(StoreError::NotFound)
        ));
        assert!(store
            .update_draft(99, Some("x".into()), None)
            .await
            .unwrap()
            .is_none());
        assert!(!store.delete_draft(99).await.unwrap());
    }
}
