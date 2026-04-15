/// Wire framing benchmarks.
///
/// Measures the throughput of the 4-byte length-prefix framing layer
/// (spec §7.1) — how many messages/second can the daemon push through
/// a Unix socket pair.
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::runtime::Runtime;

use lip::daemon::session::{read_message, write_message};
use lip::query_graph::{ErrorCode, ServerMessage};

fn make_rt() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn make_message(payload_bytes: usize) -> ServerMessage {
    ServerMessage::Error {
        message: "x".repeat(payload_bytes),
        code: ErrorCode::Internal,
    }
}

// ── Single message round-trip ─────────────────────────────────────────────────

fn bench_single_roundtrip(c: &mut Criterion) {
    let rt = make_rt();
    let mut group = c.benchmark_group("framing/single_roundtrip");

    for size in [64usize, 1_024, 65_536] {
        let msg = make_message(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("payload_bytes", size), &msg, |b, msg| {
            b.to_async(&rt).iter(|| async {
                let (mut a, mut b_sock) = tokio::net::UnixStream::pair().unwrap();
                let msg_clone = msg.clone();
                let w = tokio::spawn(async move {
                    write_message(&mut a, &msg_clone).await.unwrap();
                });
                let bytes = read_message(&mut b_sock).await.unwrap();
                w.await.unwrap();
                black_box(bytes)
            });
        });
    }
    group.finish();
}

// ── Serialisation cost ────────────────────────────────────────────────────────

fn bench_serialize(c: &mut Criterion) {
    // Isolates the serde_json serialization cost from socket I/O.
    let mut group = c.benchmark_group("framing/serialize_json");

    for size in [64usize, 1_024, 65_536] {
        let msg = make_message(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("payload_bytes", size), &msg, |b, msg| {
            b.iter(|| black_box(serde_json::to_vec(black_box(msg)).unwrap()));
        });
    }
    group.finish();
}

// ── Burst throughput ──────────────────────────────────────────────────────────

fn bench_burst(c: &mut Criterion) {
    // Send N messages back-to-back; measures framing overhead per message.
    let rt = make_rt();
    let mut group = c.benchmark_group("framing/burst");

    for n in [10usize, 100, 1_000] {
        let msg = make_message(256); // typical small query response
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("messages", n), &n, |b, &n| {
            b.to_async(&rt).iter(|| async {
                let (mut writer, mut reader) = tokio::net::UnixStream::pair().unwrap();
                let msg_clone = msg.clone();
                let w = tokio::spawn(async move {
                    for _ in 0..n {
                        write_message(&mut writer, &msg_clone).await.unwrap();
                    }
                });
                let mut total = 0usize;
                for _ in 0..n {
                    let bytes = read_message(&mut reader).await.unwrap();
                    total += bytes.len();
                }
                w.await.unwrap();
                black_box(total)
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_single_roundtrip,
    bench_serialize,
    bench_burst
);
criterion_main!(benches);
