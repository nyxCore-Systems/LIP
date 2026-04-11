//! `lip slice` — build pre-computed dependency slices for Cargo, npm, and pub packages.
//!
//! A slice is a compact, content-addressed JSON blob containing all exported symbols
//! for a specific package version. Once built and pushed to the registry, no one on
//! the team needs to re-index that dependency ever again.
//!
//! Usage:
//!   lip slice --cargo                          # uses ./Cargo.toml
//!   lip slice --cargo path/to/Cargo.toml
//!   lip slice --npm                            # uses ./package.json
//!   lip slice --pub                            # uses ./pubspec.yaml
//!   lip slice --cargo --push --registry https://registry.lip.dev

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use serde::Deserialize;
use walkdir::WalkDir;

use lip::indexer::{language::Language, Tier1Indexer};
use lip::registry::{RegistryClient, SliceCache};
use lip::schema::{sha256_hex, OwnedDependencySlice, OwnedSymbolInfo};

#[derive(Args)]
pub struct SliceArgs {
    /// Build slices for all Cargo dependencies (uses Cargo.toml in the given path).
    #[arg(long, value_name = "Cargo.toml",
          num_args = 0..=1, default_missing_value = "Cargo.toml")]
    pub cargo: Option<PathBuf>,

    /// Build slices for all npm dependencies (uses package.json in the given path).
    #[arg(long, value_name = "package.json",
          num_args = 0..=1, default_missing_value = "package.json")]
    pub npm: Option<PathBuf>,

    /// Build slices for all Dart pub dependencies (uses pubspec.yaml in the given path).
    #[arg(long, value_name = "pubspec.yaml",
          num_args = 0..=1, default_missing_value = "pubspec.yaml")]
    pub pub_dart: Option<PathBuf>,

    /// Build slices for all Python pip dependencies.
    /// Uses `pip list --format=json` to enumerate packages and `pip show` to
    /// locate their source directories. Requires `pip` in PATH.
    #[arg(long)]
    pub pip: bool,

    /// Directory to write slice files (default: ~/.cache/lip/slices).
    #[arg(long, default_value = "~/.cache/lip/slices")]
    pub output: PathBuf,

    /// Push slices to the registry after building.
    #[arg(long)]
    pub push: bool,

    /// Registry URL (used with --push).
    #[arg(long, default_value = "https://registry.lip.dev")]
    pub registry: String,
}

pub async fn run(args: SliceArgs) -> anyhow::Result<()> {
    let output = expand_home(args.output.clone());
    std::fs::create_dir_all(&output)?;

    let mut total = 0usize;

    if let Some(manifest) = &args.cargo {
        total += slice_cargo(manifest, &output, &args).await?;
    }
    if let Some(manifest) = &args.npm {
        total += slice_npm(manifest, &output, &args).await?;
    }
    if let Some(manifest) = &args.pub_dart {
        total += slice_pub(manifest, &output, &args).await?;
    }

    if args.pip {
        total += slice_pip(&output, &args).await?;
    }

    if args.cargo.is_none() && args.npm.is_none() && args.pub_dart.is_none() && !args.pip {
        anyhow::bail!(
            "specify at least one package manager: --cargo, --npm, --pub, or --pip\n\
             Example: lip slice --pip"
        );
    }

    println!("Built {total} slice(s) → {}", output.display());
    Ok(())
}

// ── Cargo ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    id: String,
    manifest_path: String,
    source: Option<String>,
}

async fn slice_cargo(manifest: &Path, output: &Path, args: &SliceArgs) -> anyhow::Result<usize> {
    println!("Slicing Cargo dependencies from {} …", manifest.display());

    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(manifest)
        .output()
        .map_err(|e| anyhow::anyhow!("cargo metadata failed: {e}\nIs cargo in PATH?"))?;

    anyhow::ensure!(
        out.status.success(),
        "cargo metadata exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let meta: CargoMetadata = serde_json::from_slice(&out.stdout)?;
    let workspace: std::collections::HashSet<&str> =
        meta.workspace_members.iter().map(String::as_str).collect();

    let deps: Vec<&CargoPackage> = meta
        .packages
        .iter()
        .filter(|p| p.source.is_some() && !workspace.contains(p.id.as_str()))
        .collect();

    let mut count = 0usize;
    for pkg in deps {
        let src_dir = PathBuf::from(&pkg.manifest_path)
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();

        if !src_dir.exists() {
            continue;
        }

        let symbols = index_directory(&src_dir, &pkg.name, &pkg.version, "cargo");
        if symbols.is_empty() {
            continue;
        }

        let slice = build_slice("cargo", &pkg.name, &pkg.version, symbols);
        let n = save_slice(&slice, output)?;
        if args.push {
            push_slice(&slice, &n, &args.registry).await?;
        }
        println!(
            "  {}@{}  ({} symbols)  {n}",
            pkg.name,
            pkg.version,
            slice.symbols.len()
        );
        count += 1;
    }

    Ok(count)
}

// ── npm ───────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    dependencies: std::collections::HashMap<String, String>,
    #[serde(rename = "devDependencies", default)]
    dev_dependencies: std::collections::HashMap<String, String>,
}

async fn slice_npm(manifest: &Path, output: &Path, args: &SliceArgs) -> anyhow::Result<usize> {
    println!("Slicing npm dependencies from {} …", manifest.display());

    let raw = std::fs::read_to_string(manifest)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", manifest.display()))?;
    let pkg: PackageJson = serde_json::from_str(&raw)?;

    let node_modules = manifest
        .parent()
        .unwrap_or(Path::new("."))
        .join("node_modules");

    anyhow::ensure!(
        node_modules.exists(),
        "node_modules not found at {} — run `npm install` first",
        node_modules.display()
    );

    let mut count = 0usize;
    let all_deps: std::collections::HashMap<&str, &str> = pkg
        .dependencies
        .iter()
        .chain(pkg.dev_dependencies.iter())
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    for (name, version_spec) in all_deps {
        let pkg_dir = node_modules.join(name);
        if !pkg_dir.exists() {
            continue;
        }

        // Resolve exact version from the package's own package.json.
        let version = read_npm_version(&pkg_dir).unwrap_or_else(|| version_spec.to_owned());

        let symbols = index_directory(&pkg_dir, name, &version, "npm");
        if symbols.is_empty() {
            continue;
        }

        let slice = build_slice("npm", name, &version, symbols);
        let hash = save_slice(&slice, output)?;
        if args.push {
            push_slice(&slice, &hash, &args.registry).await?;
        }
        println!(
            "  {name}@{version}  ({} symbols)  {hash}",
            slice.symbols.len()
        );
        count += 1;
    }

    Ok(count)
}

fn read_npm_version(pkg_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v["version"].as_str().map(str::to_owned)
}

// ── pub (Dart) ────────────────────────────────────────────────────────────────

async fn slice_pub(manifest: &Path, output: &Path, args: &SliceArgs) -> anyhow::Result<usize> {
    println!("Slicing pub dependencies from {} …", manifest.display());

    let lock_path = manifest
        .parent()
        .unwrap_or(Path::new("."))
        .join("pubspec.lock");

    anyhow::ensure!(
        lock_path.exists(),
        "pubspec.lock not found at {} — run `dart pub get` first",
        lock_path.display()
    );

    let lock_text = std::fs::read_to_string(&lock_path)?;
    let packages = parse_pubspec_lock(&lock_text);

    let pub_cache = expand_home(PathBuf::from("~/.pub-cache/hosted/pub.dev"));

    let mut count = 0usize;
    for (name, version) in packages {
        let pkg_dir = pub_cache.join(format!("{name}-{version}"));
        if !pkg_dir.exists() {
            continue;
        }

        let symbols = index_directory(&pkg_dir, &name, &version, "pub");
        if symbols.is_empty() {
            continue;
        }

        let slice = build_slice("pub", &name, &version, symbols);
        let hash = save_slice(&slice, output)?;
        if args.push {
            push_slice(&slice, &hash, &args.registry).await?;
        }
        println!(
            "  {name}@{version}  ({} symbols)  {hash}",
            slice.symbols.len()
        );
        count += 1;
    }

    Ok(count)
}

/// Minimal pubspec.lock parser — extracts (name, version) pairs.
///
/// pubspec.lock format (YAML, but we parse it line-by-line to avoid a dep):
/// ```yaml
/// packages:
///   async:
///     dependency: transitive
///     version: "2.11.0"
/// ```
fn parse_pubspec_lock(text: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut current_pkg: Option<String> = None;
    let mut in_packages = false;

    for line in text.lines() {
        if line.trim_start() == "packages:" {
            in_packages = true;
            continue;
        }
        if !in_packages {
            continue;
        }

        // Top-level keys under `packages:` are indented with exactly 2 spaces.
        if let Some(rest) = line.strip_prefix("  ") {
            if !rest.starts_with(' ') && rest.ends_with(':') {
                current_pkg = Some(rest.trim_end_matches(':').to_owned());
            }
        }

        if let Some(ref name) = current_pkg {
            if let Some(rest) = line.trim_start().strip_prefix("version:") {
                let version = rest.trim().trim_matches('"').to_owned();
                results.push((name.clone(), version));
            }
        }
    }

    results
}

// ── pip (Python) ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PipPackage {
    name: String,
    version: String,
}

async fn slice_pip(output: &Path, args: &SliceArgs) -> anyhow::Result<usize> {
    println!("Slicing pip dependencies …");

    // Step 1: enumerate all installed packages.
    let list_out = Command::new("pip")
        .args(["list", "--format=json"])
        .output()
        .map_err(|e| {
            anyhow::anyhow!("pip not found in PATH: {e}\nInstall Python and pip first.")
        })?;
    anyhow::ensure!(
        list_out.status.success(),
        "pip list failed: {}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let packages: Vec<PipPackage> = serde_json::from_slice(&list_out.stdout)
        .map_err(|e| anyhow::anyhow!("could not parse `pip list` output: {e}"))?;

    let mut count = 0usize;
    for pkg in &packages {
        // Step 2: locate the package source directory via `pip show`.
        let show_out = Command::new("pip")
            .args(["show", "--files", &pkg.name])
            .output();
        let show_out = match show_out {
            Ok(o) if o.status.success() => o,
            _ => continue,
        };
        let show_text = String::from_utf8_lossy(&show_out.stdout);

        // Extract "Location: /path/to/site-packages"
        let location = show_text
            .lines()
            .find_map(|l| l.strip_prefix("Location: ").map(str::trim))
            .map(PathBuf::from);
        let Some(location) = location else { continue };

        // The package's own directory within site-packages:
        //   - normalised name (dashes→underscores, lowercase)
        let norm = pkg.name.to_lowercase().replace('-', "_");
        // Try both the normalised name and the original.
        let pkg_dir = [norm.as_str(), pkg.name.as_str()]
            .iter()
            .map(|n| location.join(n))
            .find(|p| p.is_dir());
        let Some(pkg_dir) = pkg_dir else { continue };

        let symbols = index_directory(&pkg_dir, &pkg.name, &pkg.version, "pip");
        if symbols.is_empty() {
            continue;
        }

        let slice = build_slice("pip", &pkg.name, &pkg.version, symbols);
        let hash = save_slice(&slice, output)?;
        if args.push {
            push_slice(&slice, &hash, &args.registry).await?;
        }
        println!(
            "  {}@{}  ({} symbols)  {hash}",
            pkg.name,
            pkg.version,
            slice.symbols.len()
        );
        count += 1;
    }

    Ok(count)
}

// ── Indexing ──────────────────────────────────────────────────────────────────

/// Walk `dir` for supported source files and return all extracted symbols.
fn index_directory(
    dir: &Path,
    pkg_name: &str,
    version: &str,
    manager: &str,
) -> Vec<OwnedSymbolInfo> {
    let mut indexer = Tier1Indexer::new();
    let mut symbols = Vec::new();
    let scope = manager_scope(manager);

    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let uri = format!(
            "lip://{scope}/{pkg_name}@{version}/{}",
            path.strip_prefix(dir)
                .map(|p| p.display().to_string())
                .unwrap_or_default()
                .replace('\\', "/")
        );

        let lang = Language::detect(&uri, "");
        if lang == Language::Unknown {
            continue;
        }

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let doc = indexer.index_file(&uri, &source, lang);
        // Upgrade to Tier 3 confidence (score 100): these symbols are from a
        // published, immutable package version — equivalent to a compiler-verified
        // snapshot. Spec §3.3: Tier 3 = federated registry slice.
        symbols.extend(doc.symbols.into_iter().map(|mut s| {
            s.confidence_score = 100;
            s
        }));
    }

    symbols
}

fn manager_scope(manager: &str) -> &'static str {
    match manager {
        "cargo" => "cargo",
        "npm" => "npm",
        "pub" => "pub",
        _ => "local",
    }
}

// ── Slice construction & persistence ─────────────────────────────────────────

fn build_slice(
    manager: &str,
    name: &str,
    version: &str,
    symbols: Vec<OwnedSymbolInfo>,
) -> OwnedDependencySlice {
    let package_hash = sha256_hex(format!("{manager}:{name}@{version}").as_bytes());
    let sym_json = serde_json::to_vec(&symbols).unwrap_or_default();
    let content_hash = sha256_hex(&sym_json);
    let built_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    OwnedDependencySlice {
        manager: manager.to_owned(),
        package_name: name.to_owned(),
        version: version.to_owned(),
        package_hash,
        content_hash,
        symbols,
        slice_url: String::new(),
        built_at_ms,
    }
}

/// Serialize the slice to `{output}/{content_hash}.json`. Returns the hash.
fn save_slice(slice: &OwnedDependencySlice, output: &Path) -> anyhow::Result<String> {
    let raw = serde_json::to_vec_pretty(slice)?;
    let hash = sha256_hex(&raw);
    let path = output.join(format!("{hash}.json"));
    std::fs::write(&path, &raw)?;
    Ok(hash)
}

async fn push_slice(
    slice: &OwnedDependencySlice,
    _hash: &str,
    registry: &str,
) -> anyhow::Result<()> {
    // RegistryClient needs a local cache dir; use a temp path since we only push here.
    let cache_dir = std::env::temp_dir().join("lip-slice-push-cache");
    std::fs::create_dir_all(&cache_dir)?;
    let cache = std::sync::Arc::new(SliceCache::open(&cache_dir)?);
    let client = RegistryClient::new(vec![registry.to_owned()], cache);
    let raw = serde_json::to_vec(slice)?;
    let hash = client.push_slice(raw).await?;
    tracing::info!("pushed {}/{} ({})", slice.package_name, slice.version, hash);
    Ok(())
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn expand_home(p: PathBuf) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            return home.join(rest);
        }
    }
    p
}
