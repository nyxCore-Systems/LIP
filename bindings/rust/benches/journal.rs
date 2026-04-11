/// Journal write and replay benchmarks.
///
/// Measures the two performance-sensitive paths:
///   - Append: how fast can the daemon persist a single mutation?
///   - Replay: how fast can a daemon restart restore N entries?
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::NamedTempFile;

use lip::daemon::journal::{replay, Journal, JournalEntry};
use lip::query_graph::LipDatabase;

fn make_upsert_entry(i: usize) -> JournalEntry {
    JournalEntry::UpsertFile {
        uri: format!("lip://local/proj@0.1/file{i}.rs"),
        text: "pub fn foo() { let x = 1 + 2; x }".to_owned(),
        language: "rust".to_owned(),
    }
}

// ── Append throughput ─────────────────────────────────────────────────────────

fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("journal/append");

    // Single-entry append — the hot path on every Delta/AnnotationSet.
    group.throughput(Throughput::Elements(1));
    group.bench_function("single_upsert_entry", |b| {
        let tmp = NamedTempFile::new().unwrap();
        let (mut journal, _) = Journal::open(tmp.path()).unwrap();
        let entry = make_upsert_entry(0);
        b.iter(|| journal.append(black_box(&entry)).unwrap());
    });

    group.finish();
}

// ── Replay throughput ─────────────────────────────────────────────────────────

fn bench_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("journal/replay");

    for n in [10usize, 100, 1_000] {
        // Pre-build the entry list outside the timed section.
        let entries: Vec<JournalEntry> = (0..n).map(make_upsert_entry).collect();

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("entries", n), &entries, |b, entries| {
            b.iter(|| {
                let mut db = LipDatabase::new();
                replay(black_box(entries), &mut db);
                black_box(db.file_count())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_append, bench_replay);
criterion_main!(benches);
