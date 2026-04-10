use std::path::PathBuf;

use clap::Args;
use walkdir::WalkDir;

use lip::indexer::{language::Language, Tier1Indexer};
use lip::schema::{OwnedDelta, OwnedEventStream, Action};

use crate::output;

/// Index a directory with the Tier 1 tree-sitter indexer and emit deltas.
#[derive(Args)]
pub struct IndexArgs {
    /// Root directory to index (default: current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Language hint; auto-detected from file extension if omitted.
    #[arg(long)]
    pub language: Option<String>,

    /// Emit output as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,

    /// Maximum number of files to index (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
}

pub async fn run(args: IndexArgs) -> anyhow::Result<()> {
    let mut indexer  = Tier1Indexer::new();
    let mut deltas   = vec![];
    let mut count    = 0usize;
    let lang_hint    = args.language.as_deref().unwrap_or("");

    for entry in WalkDir::new(&args.path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        // Canonicalize to an absolute path so the URI is always well-formed.
        // WalkDir may return relative paths (e.g. `./src/main.rs`) when the
        // root was given as `.`, which would produce `file://./src/main.rs`.
        let abs = match path.canonicalize() {
            Ok(p)  => p,
            Err(_) => continue,
        };
        let uri  = format!("file://{}", abs.display());
        let lang = Language::detect(&uri, lang_hint);

        if lang == Language::Unknown {
            continue;
        }

        let source = match std::fs::read_to_string(&abs) {
            Ok(s)  => s,
            Err(_) => continue,
        };

        let doc = indexer.index_file(&uri, &source, lang);

        if args.json {
            let delta = OwnedDelta {
                action:      Action::Upsert,
                commit_hash: doc.content_hash.clone(),
                document:    Some(doc),
                symbol:      None,
                slice:       None,
            };
            deltas.push(delta);
        } else {
            let sym_count = doc.symbols.len();
            println!("{uri}  ({sym_count} symbols, lang={})", lang.as_str());
        }

        count += 1;
        if args.limit > 0 && count >= args.limit {
            break;
        }
    }

    if args.json {
        let stream = OwnedEventStream::new(
            concat!("lip-cli/", env!("CARGO_PKG_VERSION")),
            deltas,
        );
        output::print_json(&stream)?;
    } else {
        println!("\nIndexed {count} files.");
    }

    Ok(())
}
