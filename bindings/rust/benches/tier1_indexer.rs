/// Tier 1 indexer throughput benchmarks.
///
/// Validates the spec claim "< 10 ms per file" for the tree-sitter indexer
/// across Rust, TypeScript, and Python.
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use lip::indexer::{language::Language, Tier1Indexer};

// ─── Source fixtures ──────────────────────────────────────────────────────────
// Each fixture is a representative ~50-line file for its language.

const RUST_SRC: &str = r#"
use std::collections::HashMap;

pub struct Config {
    host:    String,
    port:    u16,
    timeout: std::time::Duration,
}

impl Config {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port, timeout: std::time::Duration::from_secs(30) }
    }

    pub fn host(&self) -> &str { &self.host }
    pub fn port(&self) -> u16  { self.port }
}

pub trait Handler: Send + Sync {
    fn handle(&self, req: Request) -> Response;
    fn name(&self) -> &str;
}

pub struct Request {
    pub method: String,
    pub path:   String,
    pub headers: HashMap<String, String>,
    pub body:   Vec<u8>,
}

pub struct Response {
    pub status:  u16,
    pub headers: HashMap<String, String>,
    pub body:    Vec<u8>,
}

pub fn route(handlers: &[Box<dyn Handler>], req: Request) -> Response {
    for h in handlers {
        if req.path.starts_with(h.name()) {
            return h.handle(req);
        }
    }
    Response { status: 404, headers: HashMap::new(), body: b"not found".to_vec() }
}

pub enum Error {
    Io(std::io::Error),
    Parse(String),
    NotFound { path: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e)          => write!(f, "io: {e}"),
            Error::Parse(s)       => write!(f, "parse: {s}"),
            Error::NotFound{path} => write!(f, "not found: {path}"),
        }
    }
}
"#;

const TS_SRC: &str = r#"
import { EventEmitter } from 'events';

export interface Config {
    host: string;
    port: number;
    timeout?: number;
}

export class HttpClient extends EventEmitter {
    private config: Config;
    private baseUrl: string;

    constructor(config: Config) {
        super();
        this.config  = config;
        this.baseUrl = `http://${config.host}:${config.port}`;
    }

    async get<T>(path: string): Promise<T> {
        const url  = `${this.baseUrl}${path}`;
        const resp = await fetch(url, { signal: AbortSignal.timeout(this.config.timeout ?? 5000) });
        if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${url}`);
        return resp.json() as Promise<T>;
    }

    async post<T>(path: string, body: unknown): Promise<T> {
        const url  = `${this.baseUrl}${path}`;
        const resp = await fetch(url, {
            method:  'POST',
            headers: { 'Content-Type': 'application/json' },
            body:    JSON.stringify(body),
        });
        if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${url}`);
        return resp.json() as Promise<T>;
    }
}

export type Result<T> = { ok: true; value: T } | { ok: false; error: string };

export function wrapResult<T>(fn: () => T): Result<T> {
    try   { return { ok: true,  value: fn() }; }
    catch (e) { return { ok: false, error: String(e) }; }
}
"#;

const PY_SRC: &str = r#"
from __future__ import annotations
from typing import Any, Dict, List, Optional
from dataclasses import dataclass, field
import asyncio

@dataclass
class Config:
    host:    str
    port:    int
    timeout: float = 30.0
    headers: Dict[str, str] = field(default_factory=dict)

class HttpError(Exception):
    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status

class Client:
    def __init__(self, config: Config) -> None:
        self._config  = config
        self._session: Optional[Any] = None

    @property
    def base_url(self) -> str:
        return f"http://{self._config.host}:{self._config.port}"

    async def get(self, path: str) -> Dict[str, Any]:
        url = f"{self.base_url}{path}"
        async with self._session.get(url) as resp:
            if resp.status >= 400:
                raise HttpError(resp.status, f"GET {url} failed")
            return await resp.json()

    async def post(self, path: str, body: Dict[str, Any]) -> Dict[str, Any]:
        url = f"{self.base_url}{path}"
        async with self._session.post(url, json=body) as resp:
            if resp.status >= 400:
                raise HttpError(resp.status, f"POST {url} failed")
            return await resp.json()

def retry(max_attempts: int = 3):
    def decorator(fn):
        async def wrapper(*args, **kwargs):
            for attempt in range(max_attempts):
                try:
                    return await fn(*args, **kwargs)
                except HttpError as e:
                    if attempt == max_attempts - 1 or e.status < 500:
                        raise
            return None
        return wrapper
    return decorator
"#;

// ─── Benchmarks ───────────────────────────────────────────────────────────────

fn bench_symbols(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1/symbols");

    let cases: &[(&str, &str, Language)] = &[
        ("rust", "file.rs", Language::Rust),
        ("typescript", "file.ts", Language::TypeScript),
        ("python", "file.py", Language::Python),
    ];

    for (name, uri, lang) in cases {
        let src = match *lang {
            Language::Rust => RUST_SRC,
            Language::TypeScript => TS_SRC,
            Language::Python => PY_SRC,
            _ => unreachable!(),
        };
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("symbols_for_source", name),
            src,
            |b, src| {
                b.iter(|| {
                    let mut idx = Tier1Indexer::new();
                    black_box(idx.symbols_for_source(uri, black_box(src), *lang))
                })
            },
        );
    }
    group.finish();
}

fn bench_occurrences(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1/occurrences");

    let cases: &[(&str, &str, Language)] = &[
        ("rust", "file.rs", Language::Rust),
        ("typescript", "file.ts", Language::TypeScript),
        ("python", "file.py", Language::Python),
    ];

    for (name, uri, lang) in cases {
        let src = match *lang {
            Language::Rust => RUST_SRC,
            Language::TypeScript => TS_SRC,
            Language::Python => PY_SRC,
            _ => unreachable!(),
        };
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("occurrences_for_source", name),
            src,
            |b, src| {
                b.iter(|| {
                    let mut idx = Tier1Indexer::new();
                    black_box(idx.occurrences_for_source(uri, black_box(src), *lang))
                })
            },
        );
    }
    group.finish();
}

fn bench_index_file(c: &mut Criterion) {
    // index_file runs both symbols + occurrences — validates the combined
    // "< 10 ms per file" spec claim.
    let mut group = c.benchmark_group("tier1/index_file");

    let cases: &[(&str, &str, Language)] = &[
        ("rust", "file.rs", Language::Rust),
        ("typescript", "file.ts", Language::TypeScript),
        ("python", "file.py", Language::Python),
    ];

    for (name, uri, lang) in cases {
        let src = match *lang {
            Language::Rust => RUST_SRC,
            Language::TypeScript => TS_SRC,
            Language::Python => PY_SRC,
            _ => unreachable!(),
        };
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::new("index_file", name), src, |b, src| {
            b.iter(|| {
                let mut idx = Tier1Indexer::new();
                black_box(idx.index_file(uri, black_box(src), *lang))
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_symbols, bench_occurrences, bench_index_file);
criterion_main!(benches);
