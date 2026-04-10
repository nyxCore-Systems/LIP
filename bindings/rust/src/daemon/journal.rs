//! Append-only write-ahead journal for the LIP daemon (spec §6.7).
//!
//! Every mutation to [`LipDatabase`](crate::query_graph::LipDatabase) is
//! appended to a newline-delimited JSON file before the mutation is applied.
//! On restart the daemon replays all entries to restore in-memory state.
//!
//! ## Format
//!
//! One JSON object per line. Each line carries a `"op"` discriminant tag.
//!
//! ```text
//! {"op":"upsert_file","uri":"lip://local/…","text":"…","language":"rust"}
//! {"op":"remove_file","uri":"lip://local/…"}
//! {"op":"set_merkle_root","root":"abc123"}
//! {"op":"annotation_set","entry":{…}}
//! ```
//!
//! Partial lines (from a crash mid-write) are skipped on replay with a warning;
//! they cannot corrupt already-persisted state.
//!
//! ## Compaction
//!
//! After replay on startup, [`compact`] rewrites the journal as a minimal
//! snapshot of current db state — one entry per live file, one per annotation.
//! This prevents unbounded growth when files are repeatedly upserted. The
//! rewrite is atomic (write to `.journal.tmp`, then `rename`).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::query_graph::LipDatabase;
use crate::schema::OwnedAnnotationEntry;

/// Compact the journal when it has accumulated this many entries.
/// Below this threshold the overhead of compaction isn't worth it.
pub const COMPACT_THRESHOLD: usize = 500;

// ─── Entry types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalEntry {
    UpsertFile    { uri: String, text: String, language: String },
    RemoveFile    { uri: String },
    SetMerkleRoot { root: String },
    SetWorkspaceRoot { path: String },
    AnnotationSet { entry: OwnedAnnotationEntry },
}

// ─── Journal ─────────────────────────────────────────────────────────────────

pub struct Journal {
    file: File,
}

impl Journal {
    /// Open (or create) the journal at `path`.
    ///
    /// Returns `(journal, entries_to_replay)`. Malformed lines are skipped with
    /// a warning so a truncated final write on crash does not break replay.
    pub fn open(path: &Path) -> anyhow::Result<(Self, Vec<JournalEntry>)> {
        let entries = if path.exists() {
            let reader = BufReader::new(File::open(path)?);
            reader
                .lines()
                .enumerate()
                .filter_map(|(i, line)| match line {
                    Err(e) => {
                        warn!("journal I/O error at line {i}: {e}");
                        None
                    }
                    Ok(l) if l.trim().is_empty() => None,
                    Ok(l) => match serde_json::from_str::<JournalEntry>(&l) {
                        Ok(entry) => Some(entry),
                        Err(e) => {
                            warn!("journal parse error at line {i}: {e}");
                            None
                        }
                    },
                })
                .collect()
        } else {
            vec![]
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        Ok((Self { file }, entries))
    }

    /// Open (or create) `path` for appending only, without reading existing entries.
    ///
    /// Use this after [`compact`] has already rewritten the file — the entries
    /// on disk are the current db state, no replay is needed.
    pub fn open_append(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file })
    }

    /// Append a single entry. Flushes immediately so a crash between writes
    /// can only lose the in-flight entry, never corrupt earlier ones.
    pub fn append(&mut self, entry: &JournalEntry) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}

// ─── Compaction ───────────────────────────────────────────────────────────────

/// Rewrite `path` as a minimal snapshot of `db`'s current state.
///
/// The rewrite is atomic: entries are written to `<path>.tmp`, then that file
/// is renamed over `path`. On success the journal is as short as possible —
/// one `UpsertFile` per tracked file, one `AnnotationSet` per annotation, plus
/// lifecycle entries. Pending in-flight appends will see the new file after the
/// rename; no journal entries are lost.
pub fn compact(path: &Path, db: &LipDatabase) -> anyhow::Result<usize> {
    let tmp_path = {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        path.with_file_name(format!("{name}.tmp"))
    };

    let mut count = 0usize;
    {
        let tmp_file = File::create(&tmp_path)?;
        let mut w    = BufWriter::new(tmp_file);

        let mut write_entry = |entry: &JournalEntry| -> anyhow::Result<()> {
            let mut line = serde_json::to_string(entry)?;
            line.push('\n');
            w.write_all(line.as_bytes())?;
            count += 1;
            Ok(())
        };

        // Lifecycle state.
        if let Some(root) = db.current_merkle_root() {
            write_entry(&JournalEntry::SetMerkleRoot { root: root.to_owned() })?;
        }
        if let Some(ws) = db.workspace_root() {
            write_entry(&JournalEntry::SetWorkspaceRoot {
                path: ws.to_string_lossy().into_owned(),
            })?;
        }

        // One UpsertFile per tracked file.
        for uri in db.tracked_uris() {
            if let (Some(text), Some(lang)) = (db.file_text(&uri), db.file_language(&uri)) {
                write_entry(&JournalEntry::UpsertFile {
                    uri,
                    text:     text.to_owned(),
                    language: lang.to_owned(),
                })?;
            }
        }

        // All annotations.
        for entry in db.all_annotations() {
            write_entry(&JournalEntry::AnnotationSet { entry })?;
        }

        w.flush()?;
    }

    // Atomic rename — on POSIX this is guaranteed to be atomic.
    std::fs::rename(&tmp_path, path)?;
    Ok(count)
}

// ─── Replay ──────────────────────────────────────────────────────────────────

/// Apply a slice of journal entries to `db`, restoring its pre-shutdown state.
///
/// This is the only code path that calls db mutations without writing back to
/// the journal — the entries are already on disk.
pub fn replay(entries: &[JournalEntry], db: &mut LipDatabase) {
    for entry in entries {
        match entry {
            JournalEntry::UpsertFile { uri, text, language } => {
                db.upsert_file(uri.clone(), text.clone(), language.clone());
            }
            JournalEntry::RemoveFile { uri } => {
                db.remove_file(uri);
            }
            JournalEntry::SetMerkleRoot { root } => {
                db.set_merkle_root(root.clone());
            }
            JournalEntry::SetWorkspaceRoot { path } => {
                db.set_workspace_root(std::path::PathBuf::from(path));
            }
            JournalEntry::AnnotationSet { entry } => {
                db.annotation_set(entry.clone());
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn roundtrip_upsert_and_remove() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();
        // Remove so Journal::open creates it fresh (NamedTempFile already
        // creates the file, which is fine — open will read 0 lines).

        let (mut j, entries) = Journal::open(&path).unwrap();
        assert!(entries.is_empty());

        j.append(&JournalEntry::UpsertFile {
            uri:      "lip://local/src/main.rs".into(),
            text:     "fn main() {}".into(),
            language: "rust".into(),
        })
        .unwrap();
        j.append(&JournalEntry::RemoveFile { uri: "lip://local/src/main.rs".into() })
            .unwrap();
        drop(j);

        let (_j2, entries) = Journal::open(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], JournalEntry::UpsertFile { uri, .. } if uri == "lip://local/src/main.rs"));
        assert!(matches!(&entries[1], JournalEntry::RemoveFile { uri } if uri == "lip://local/src/main.rs"));
    }

    #[test]
    fn malformed_line_is_skipped() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        // Write one good line, one bad line, one good line.
        {
            let mut f = OpenOptions::new().write(true).open(&path).unwrap();
            f.write_all(b"{\"op\":\"remove_file\",\"uri\":\"lip://local/a.rs\"}\n").unwrap();
            f.write_all(b"THIS IS NOT JSON\n").unwrap();
            f.write_all(b"{\"op\":\"set_merkle_root\",\"root\":\"deadbeef\"}\n").unwrap();
        }

        let (_j, entries) = Journal::open(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], JournalEntry::RemoveFile { .. }));
        assert!(matches!(&entries[1], JournalEntry::SetMerkleRoot { root } if root == "deadbeef"));
    }

    #[test]
    fn replay_restores_db_state() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        let (mut j, _) = Journal::open(&path).unwrap();
        j.append(&JournalEntry::UpsertFile {
            uri:      "lip://local/foo.rs".into(),
            text:     "pub fn foo() {}".into(),
            language: "rust".into(),
        })
        .unwrap();
        j.append(&JournalEntry::SetMerkleRoot { root: "cafebabe".into() }).unwrap();
        drop(j);

        let (_j2, entries) = Journal::open(&path).unwrap();
        let mut db = LipDatabase::new();
        replay(&entries, &mut db);

        assert_eq!(db.file_count(), 1);
        assert_eq!(db.current_merkle_root(), Some("cafebabe"));
        assert!(!db.file_symbols("lip://local/foo.rs").is_empty());
    }

    #[test]
    fn compact_reduces_entry_count() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        // Write 5 upserts for the same file (simulates repeated edits).
        let (mut j, _) = Journal::open(&path).unwrap();
        for i in 0..5 {
            j.append(&JournalEntry::UpsertFile {
                uri:      "lip://local/a.rs".into(),
                text:     format!("pub fn v{i}() {{}}"),
                language: "rust".into(),
            }).unwrap();
        }
        j.append(&JournalEntry::SetMerkleRoot { root: "abc".into() }).unwrap();
        drop(j);

        // Replay into a db then compact.
        let (_, entries) = Journal::open(&path).unwrap();
        assert_eq!(entries.len(), 6, "should have 6 raw entries before compaction");

        let mut db = LipDatabase::new();
        replay(&entries, &mut db);
        let n = compact(&path, &db).unwrap();

        // After compaction: 1 UpsertFile + 1 SetMerkleRoot = 2 entries.
        assert_eq!(n, 2, "compacted journal should have 2 entries, got {n}");

        // Re-open and replay the compacted journal — db state should be identical.
        let (_, compacted_entries) = Journal::open(&path).unwrap();
        assert_eq!(compacted_entries.len(), 2);

        let mut db2 = LipDatabase::new();
        replay(&compacted_entries, &mut db2);
        assert_eq!(db2.file_count(), 1);
        assert_eq!(db2.current_merkle_root(), Some("abc"));
    }

    #[test]
    fn open_append_creates_file_if_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.journal");
        assert!(!path.exists());
        let mut j = Journal::open_append(&path).unwrap();
        j.append(&JournalEntry::RemoveFile { uri: "lip://local/x.rs".into() }).unwrap();
        assert!(path.exists());
    }
}
