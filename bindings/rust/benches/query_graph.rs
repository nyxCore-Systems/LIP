/// Query graph performance benchmarks.
///
/// Measures the cost of the key operations:
///   - `upsert_file` (revision bump)
///   - cache hit path  (no recomputation)
///   - cache miss path (full recomputation)
///   - early-cutoff    (same content, same Arc returned)
///   - `blast_radius_for` on a multi-file workspace
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use lip_core::query_graph::LipDatabase;

const RUST_SRC: &str = r#"
pub struct Foo { x: i32 }
impl Foo {
    pub fn new(x: i32) -> Self { Self { x } }
    pub fn value(&self) -> i32 { self.x }
    fn private_helper(&self) -> i32 { self.x * 2 }
}
pub fn create(n: i32) -> Foo { Foo::new(n) }
pub trait Trait { fn method(&self) -> i32; }
impl Trait for Foo { fn method(&self) -> i32 { self.value() } }
"#;

fn make_db_with_files(n: usize) -> LipDatabase {
    let mut db = LipDatabase::new();
    for i in 0..n {
        db.upsert_file(
            format!("lip://local/proj@0.1/{i}.rs"),
            RUST_SRC.to_owned(),
            "rust".to_owned(),
        );
    }
    db
}

// ── upsert_file ───────────────────────────────────────────────────────────────

fn bench_upsert(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_graph/upsert_file");

    for n in [1usize, 10, 100] {
        group.bench_with_input(BenchmarkId::new("files_already_tracked", n), &n, |b, &n| {
            let mut db = make_db_with_files(n);
            b.iter(|| {
                db.upsert_file(
                    black_box("lip://local/proj@0.1/new.rs".to_owned()),
                    black_box(RUST_SRC.to_owned()),
                    black_box("rust".to_owned()),
                );
            });
        });
    }
    group.finish();
}

// ── file_symbols: cache hit vs miss ───────────────────────────────────────────

fn bench_file_symbols(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_graph/file_symbols");
    let uri = "lip://local/proj@0.1/lib.rs".to_owned();

    group.bench_function("cache_miss_first_call", |b| {
        b.iter(|| {
            // Fresh db each iteration — always a cache miss.
            let mut db = LipDatabase::new();
            db.upsert_file(uri.clone(), RUST_SRC.to_owned(), "rust".to_owned());
            black_box(db.file_symbols(black_box(&uri)))
        });
    });

    group.bench_function("cache_hit_second_call", |b| {
        // Warm the cache once before the timed section.
        let mut db = LipDatabase::new();
        db.upsert_file(uri.clone(), RUST_SRC.to_owned(), "rust".to_owned());
        let _ = db.file_symbols(&uri); // warm
        b.iter(|| black_box(db.file_symbols(black_box(&uri))));
    });

    group.finish();
}

// ── api_surface early-cutoff ──────────────────────────────────────────────────

fn bench_api_surface(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_graph/file_api_surface");
    let uri = "lip://local/proj@0.1/lib.rs".to_owned();

    group.bench_function("early_cutoff_same_content", |b| {
        let mut db = LipDatabase::new();
        db.upsert_file(uri.clone(), RUST_SRC.to_owned(), "rust".to_owned());
        let _ = db.file_api_surface(&uri); // warm

        b.iter(|| {
            // Re-upsert the same content, then re-query — early-cutoff should fire.
            db.upsert_file(uri.clone(), RUST_SRC.to_owned(), "rust".to_owned());
            black_box(db.file_api_surface(black_box(&uri)))
        });
    });

    group.bench_function("full_recompute_changed_content", |b| {
        let mut db = LipDatabase::new();
        db.upsert_file(uri.clone(), RUST_SRC.to_owned(), "rust".to_owned());
        let _ = db.file_api_surface(&uri);

        let alt = RUST_SRC.replace("pub fn create", "pub fn make");
        let mut toggle = false;

        b.iter(|| {
            toggle = !toggle;
            let src = if toggle { RUST_SRC } else { alt.as_str() };
            db.upsert_file(uri.clone(), src.to_owned(), "rust".to_owned());
            black_box(db.file_api_surface(black_box(&uri)))
        });
    });

    group.finish();
}

// ── blast_radius ──────────────────────────────────────────────────────────────

fn bench_blast_radius(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_graph/blast_radius");

    for n in [5usize, 20, 50] {
        group.bench_with_input(BenchmarkId::new("workspace_size", n), &n, |b, &n| {
            let mut db = make_db_with_files(n);
            let uri = "lip://local/proj@0.1/0.rs";
            // Ensure the symbol is indexed.
            let syms = db.file_symbols(uri);
            let sym_uri = syms
                .first()
                .map(|s| s.uri.clone())
                .unwrap_or_else(|| "lip://local/proj@0.1/0.rs#Foo".to_owned());

            b.iter(|| black_box(db.blast_radius_for(black_box(&sym_uri))));
        });
    }
    group.finish();
}

// ── workspace_symbols ─────────────────────────────────────────────────────────

fn bench_workspace_symbols(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_graph/workspace_symbols");

    for n in [10usize, 50, 100] {
        group.bench_with_input(BenchmarkId::new("files", n), &n, |b, &n| {
            let mut db = make_db_with_files(n);
            b.iter(|| black_box(db.workspace_symbols(black_box("Foo"), 50)));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_upsert,
    bench_file_symbols,
    bench_api_surface,
    bench_blast_radius,
    bench_workspace_symbols,
);
criterion_main!(benches);
