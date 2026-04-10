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

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::query_graph::LipDatabase;
use crate::schema::OwnedAnnotationEntry;

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
}
