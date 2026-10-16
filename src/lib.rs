mod autoload;
mod cache;
mod filters;
mod html;
mod model;
mod php;
mod profile;
mod scheduler;
mod sql;
mod store;

use anyhow::{Context, Result, bail};
use clap::Args;
use model::Node;
use serde::Serialize;
use std::{
    fs,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use store::Store;

#[derive(Args, Debug, Clone)]
pub struct Options {
    /// Project root. Paths in the graph are relative to this directory.
    #[arg(default_value = ".")]
    pub root: PathBuf,
    /// Output directory (default: <root>/graphify-out).
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// Optional persistent cache directory for raw file facts; resolution runs fresh.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Root-relative glob; repeat to add patterns. Explicit exclusions always win.
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Exact root-relative file path, not a glob. May be repeated.
    #[arg(long)]
    pub exclude_file: Vec<PathBuf>,
    /// UTF-8 file containing one root-relative exclusion glob per line.
    #[arg(long)]
    pub exclude_from: Vec<PathBuf>,
    /// Concurrent parsing workers (1 keeps the lowest memory use).
    #[arg(long, default_value_t = 1)]
    pub workers: usize,
    /// Concurrent input admission estimate in MiB; oversized files run alone.
    /// Parser/fact expansion means this is not a process memory limit.
    #[arg(long, default_value_t = 64)]
    pub parse_input_mib: usize,
    /// Maximum PHP/HTML file size and buffered SQL statement size in MiB.
    #[arg(long, default_value_t = 16)]
    pub max_file_mib: usize,
    /// SQL dialect: auto (directory hints, otherwise MySQL), mysql, postgres, sqlite, generic.
    #[arg(long, default_value = "auto", value_parser = ["auto", "mysql", "postgres", "sqlite", "generic"])]
    pub sql_dialect: String,
    /// SQL context override GLOB=NAME; use @file to isolate each matching file. Repeatable.
    #[arg(long)]
    pub sql_scope: Vec<String>,
    /// Disable gitignore filtering (explicit exclusions still apply).
    #[arg(long)]
    pub no_gitignore: bool,
}

impl Options {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            out: None,
            cache_dir: None,
            exclude: vec![],
            exclude_file: vec![],
            exclude_from: vec![],
            workers: 1,
            parse_input_mib: 64,
            max_file_mib: 16,
            sql_dialect: "auto".into(),
            sql_scope: vec![],
            no_gitignore: false,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub files: u64,
    pub nodes: u64,
    pub edges: u64,
    pub unresolved_references: u64,
    pub skipped_sql_data_statements: u64,
    pub ignored_sql_statements: u64,
    pub dynamic_php_references: u64,
    pub skipped_html_references: u64,
    /// Files carrying a Hack opening tag: inventoried explicitly, not parsed as PHP.
    pub unsupported_hack_files: u64,
    pub inferred_edges: u64,
}

pub fn extract(options: &Options) -> Result<Stats> {
    if options.workers == 0 {
        bail!("workers must be positive");
    }
    let root = options.root.canonicalize().context("open project root")?;
    if !root.is_dir() {
        bail!("project root must be a directory");
    }
    let max_bytes = options
        .max_file_mib
        .checked_mul(1024 * 1024)
        .filter(|n| *n > 0)
        .context("max-file-mib must be positive and fit in memory address space")?;
    let parse_input_bytes = options
        .parse_input_mib
        .checked_mul(1024 * 1024)
        .filter(|n| *n > 0)
        .context("parse-input-mib must be positive and fit in memory address space")?;
    let filters = filters::Filters::new(&root, options)?;
    let sql_settings = sql::Settings::new(&options.sql_dialect, &options.sql_scope)?;
    let out = options
        .out
        .clone()
        .unwrap_or_else(|| root.join("graphify-out"));
    fs::create_dir_all(&out).context("create output directory")?;
    let out = out.canonicalize()?;
    if root.starts_with(&out) {
        bail!("output must not be the project root or its ancestor");
    }
    let cache_totals = Arc::new(Mutex::new(cache::Telemetry::default()));
    let cache_dir = if let Some(path) = &options.cache_dir {
        // This directory is excluded from discovery. Reject a root/ancestor
        // before opening any cache database to prevent an incomplete inventory.
        if path.canonicalize().is_ok_and(|path| root.starts_with(path)) {
            bail!("cache directory must not be the project root or its ancestor");
        }
        match fs::create_dir_all(path).and_then(|()| path.canonicalize()) {
            Ok(path) => {
                if root.starts_with(&path) {
                    bail!("cache directory must not be the project root or its ancestor");
                }
                Some(path)
            }
            Err(error) => {
                eprintln!("file-fact cache unavailable: {error}");
                cache_totals.lock().unwrap().errors += 1;
                None
            }
        }
    } else {
        None
    };
    // Each full rebuild has its own temporary store. Never reuse an incomplete cache.
    let workspace = tempfile::tempdir_in(&out)?;
    let store = Store::open(&workspace.path().join("facts.sqlite"))?;
    let mut stats = Stats::default();
    let walker_root = root.clone();
    let walker_out = out.clone();
    let walker_cache = cache_dir.clone();
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .git_ignore(!options.no_gitignore)
        .git_global(false)
        .git_exclude(!options.no_gitignore)
        .ignore(false)
        .parents(false)
        .filter_entry(move |entry| {
            let path = entry.path();
            if path == walker_root {
                return true;
            }
            !path.starts_with(&walker_out)
                && !walker_cache
                    .as_ref()
                    .is_some_and(|cache| path.starts_with(cache))
                && entry.file_name() != ".git"
                && !filters.excludes(path, &walker_root)
        });
    // Bound both the pending paths and disk-backed results. No project-wide
    // fact vectors and no shared SQLite connection in parser threads.
    let entries = builder.build();
    if options.workers == 1 {
        let mut cache = cache::Session::new(
            cache_dir.as_deref(),
            options,
            max_bytes,
            cache_totals.clone(),
        );
        store.begin()?;
        for entry in entries {
            let entry = entry.context("cannot completely enumerate project")?;
            if entry.file_type().is_some_and(|t| t.is_file()) {
                extract_file(
                    entry.path(),
                    &root,
                    &store,
                    &sql_settings,
                    max_bytes,
                    &mut stats,
                    &mut cache,
                    workspace.path(),
                )?;
            }
        }
        store.commit()?;
    } else {
        let paths = entries.filter_map(|entry| {
            let entry = match entry.context("cannot completely enumerate project") {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error)),
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) || language(entry.path()).is_none() {
                return None;
            }
            // SQL streams statements, so charge at most its buffered statement
            // limit. All parsers use this admitted size as their read/buffer
            // limit, so concurrent growth cannot escape input admission.
            let charge = entry
                .metadata()
                .with_context(|| format!("inspect {} for input admission", entry.path().display()))
                .map(|metadata| metadata.len().min(max_bytes as u64) as usize);
            Some(charge.map(|bytes| (entry.into_path(), bytes)))
        });
        scheduler::extract(
            paths,
            options.workers,
            parse_input_bytes,
            workspace.path(),
            || {
                let mut cache = cache::Session::new(
                    cache_dir.as_deref(),
                    options,
                    max_bytes,
                    cache_totals.clone(),
                );
                let root = &root;
                let sql_settings = &sql_settings;
                let workspace = workspace.path();
                Ok(
                    move |path: &Path, admitted: usize, local: &Store, stats: &mut Stats| {
                        extract_file(
                            path,
                            root,
                            local,
                            sql_settings,
                            admitted,
                            stats,
                            &mut cache,
                            workspace,
                        )
                    },
                )
            },
            |path, local| {
                store.merge(path)?;
                stats.files += local.files;
                stats.skipped_sql_data_statements += local.skipped_sql_data_statements;
                stats.ignored_sql_statements += local.ignored_sql_statements;
                stats.dynamic_php_references += local.dynamic_php_references;
                stats.skipped_html_references += local.skipped_html_references;
                stats.unsupported_hack_files += local.unsupported_hack_files;
                Ok(())
            },
        )?;
    }
    store.begin()?;
    let resolve_span = profile::span(|| "resolve".into());
    store.load_autoload(&root, max_bytes)?;
    stats.unresolved_references = store.resolve()?;
    drop(resolve_span);
    store.commit()?;
    (stats.nodes, stats.edges) = store.counts()?;
    stats.inferred_edges = store.inferred_edges()?;
    let metadata = serde_json::json!({
        "producer": "graphify-rust-extractor", "version": env!("CARGO_PKG_VERSION"),
        "scope": "structural", "id_scheme": "gre-sha256-v1", "stats": &stats,
        "sql_dialect": options.sql_dialect, "sql_mode": "schema", "incremental": false,
        "sql_scope_rules": options.sql_scope,
        "sql_resolution": "same file, then unique dialect/context/schema target; no cross-context fallback",
        "sql_auto_policy": "nearest dialect directory, MySQL fallback, ANSI line comments",
        "php_resolution": "same-file declarations, then static Composer context and exact qualified symbols; never execute project PHP",
        "php_identifier_encoding": "UTF-8; literal U+F100 doubled, invalid source byte b escaped as U+F100 followed by U+E000+b",
        "limitations": ["no LLM semantics", "no JavaScript extraction", "receiver calls are inferred; runtime dispatch is not fully resolved",
            "SQL declarations, not executed migration state", "Hack files are inventoried as language=hack, without symbol extraction"]
    });
    let _export_span = profile::span(|| "export".into());
    let mut temp = tempfile::NamedTempFile::new_in(&out)?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        store.export(&mut writer, &metadata)?;
        writer.flush()?;
    }
    temp.as_file().sync_all()?;
    temp.persist(out.join("graph.json"))
        .context("atomically publish graph.json")?;
    if options.cache_dir.is_some() {
        eprintln!(
            "GRE_CACHE {}",
            serde_json::to_string(&*cache_totals.lock().unwrap())?
        );
    }
    Ok(stats)
}

fn language(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "php" | "phtml" | "php3" | "php4" | "php5" | "php7" | "phps" => Some("php"),
        "sql" => Some("sql"),
        "html" | "htm" => Some("html"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)] // Per-file worker context, with no project fact collection.
fn extract_file(
    path: &Path,
    root: &Path,
    store: &Store,
    sql_settings: &sql::Settings<'_>,
    max_bytes: usize,
    stats: &mut Stats,
    session: &mut cache::Session,
    workspace: &Path,
) -> Result<()> {
    let Some(lang) = language(path) else {
        return Ok(());
    };
    let rel = path
        .strip_prefix(root)?
        .to_str()
        .context("non-UTF-8 source path")?
        .replace(std::path::MAIN_SEPARATOR, "/");
    let _file_span = profile::span(|| format!("file:{rel}"));
    if let Some(cache) = session.cache(store) {
        let snapshot = cache::Snapshot::read(path, lang, max_bytes, workspace)
            .with_context(|| format!("snapshot {rel}; previous graph.json left untouched"))?;
        let key = cache.key(&rel, &snapshot);
        if cache.restore(&key, &rel, store, stats)? {
            return Ok(());
        }
        store.start_capture()?;
        let before = cache::FileStats::snapshot(stats);
        let result = extract_contents(
            snapshot.sql_path().unwrap_or(path),
            &rel,
            lang,
            snapshot.bytes(),
            store,
            sql_settings,
            max_bytes,
            stats,
        );
        store.stop_capture()?;
        result?;
        cache.save(&key, &rel, store, before.delta(stats));
        return Ok(());
    }
    // Identify the language before creating the file node. A .php suffix does
    // not make Hack syntax PHP; retain visible coverage instead of hiding it.
    let bytes = if lang != "sql" {
        Some(
            read_limited(path, max_bytes)
                .with_context(|| format!("extract {rel}; previous graph.json left untouched"))?,
        )
    } else {
        None
    };
    extract_contents(
        path,
        &rel,
        lang,
        bytes.as_deref(),
        store,
        sql_settings,
        max_bytes,
        stats,
    )
}

#[allow(clippy::too_many_arguments)] // Parse one stable source version into its disk-backed store.
fn extract_contents(
    path: &Path,
    rel: &str,
    lang: &str,
    bytes: Option<&[u8]>,
    store: &Store,
    sql_settings: &sql::Settings<'_>,
    max_bytes: usize,
    stats: &mut Stats,
) -> Result<()> {
    if lang == "php" && is_hack(bytes.unwrap()) {
        let mut file = Node::file(rel, "hack");
        file.origin = "inventory".into();
        store.node(&file)?;
        store.symbol("file", rel, &file.id)?;
        stats.unsupported_hack_files += 1;
        stats.files += 1;
        return Ok(());
    }
    let file = Node::file(rel, lang);
    store.node(&file)?;
    store.symbol("file", rel, &file.id)?;
    let result = match lang {
        "sql" => sql::extract(path, rel, &file.id, store, sql_settings, max_bytes, stats),
        "php" => php::extract(bytes.unwrap(), rel, &file.id, store, stats),
        _ => html::extract(bytes.unwrap(), rel, &file.id, store, stats),
    };
    result.with_context(|| format!("extract {rel}; previous graph.json left untouched"))?;
    stats.files += 1;
    Ok(())
}

fn is_hack(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let bytes = &bytes[start..];
    bytes.starts_with(b"<?hh") && bytes.get(4).is_none_or(u8::is_ascii_whitespace)
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit as u64 {
        bail!("file exceeds configured size limit; exclude it or raise --max-file-mib");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("file grew beyond configured size limit");
    }
    Ok(bytes)
}
