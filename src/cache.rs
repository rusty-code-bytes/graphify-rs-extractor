//! Optional, disposable cache of pre-resolution file facts. Source snapshots and
//! SQLite cursors bound memory by one source/row, never by project size.
use crate::{Options, Stats, store::Store};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

const FORMAT: u64 = 3;
pub(crate) const DATABASE: &str = "raw-facts-v1.sqlite3";

// SQLite has one writer. Queue only the existing bounded worker set here rather
// than making concurrent per-file transactions race their busy timeouts. This
// also serializes optional cache writes for unrelated projects in this process;
// parsing, snapshotting, validation and cache reads remain outside the lock.
static CACHE_WRITER: Mutex<()> = Mutex::new(());

fn writer_access() -> std::sync::MutexGuard<'static, ()> {
    CACHE_WRITER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct Telemetry {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub errors: u64,
}

impl Telemetry {
    pub fn add(&mut self, other: Self) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.writes += other.writes;
        self.errors += other.errors;
    }
}

pub(crate) struct Session {
    cache: Option<FileCache>,
    capture_ready: bool,
    totals: Arc<Mutex<Telemetry>>,
}

impl Session {
    pub fn new(
        dir: Option<&Path>,
        options: &Options,
        max_bytes: usize,
        totals: Arc<Mutex<Telemetry>>,
    ) -> Self {
        let cache = dir.and_then(|dir| match FileCache::open(dir, options, max_bytes) {
            Ok(cache) => Some(cache),
            Err(error) => {
                eprintln!("file-fact cache unavailable: {error:#}");
                totals.lock().unwrap().errors += 1;
                None
            }
        });
        Self {
            cache,
            capture_ready: false,
            totals,
        }
    }

    pub fn cache(&mut self, store: &Store) -> Option<&mut FileCache> {
        if self.cache.is_some() && !self.capture_ready {
            match store.initialize_capture() {
                Ok(()) => self.capture_ready = true,
                Err(error) => {
                    eprintln!("file-fact capture unavailable: {error:#}");
                    self.cache = None;
                    self.totals.lock().unwrap().errors += 1;
                }
            }
        }
        self.cache.as_mut()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(cache) = &self.cache {
            self.totals.lock().unwrap().add(cache.telemetry());
        }
    }
}

/// Only parser counters are reusable. Resolver counters always come from a fresh
/// complete-project resolution, including fresh static Composer metadata.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileStats {
    files: u64,
    skipped_sql_data_statements: u64,
    ignored_sql_statements: u64,
    sql_statement_only: u64,
    dynamic_php_references: u64,
    skipped_html_references: u64,
    unsupported_hack_files: u64,
}

impl FileStats {
    pub fn snapshot(stats: &Stats) -> Self {
        Self {
            files: stats.files,
            skipped_sql_data_statements: stats.skipped_sql_data_statements,
            ignored_sql_statements: stats.ignored_sql_statements,
            sql_statement_only: stats.sql_statement_only,
            dynamic_php_references: stats.dynamic_php_references,
            skipped_html_references: stats.skipped_html_references,
            unsupported_hack_files: stats.unsupported_hack_files,
        }
    }

    pub fn delta(self, stats: &Stats) -> Self {
        let after = Self::snapshot(stats);
        Self {
            files: after.files - self.files,
            skipped_sql_data_statements: after.skipped_sql_data_statements
                - self.skipped_sql_data_statements,
            ignored_sql_statements: after.ignored_sql_statements - self.ignored_sql_statements,
            sql_statement_only: after.sql_statement_only - self.sql_statement_only,
            dynamic_php_references: after.dynamic_php_references - self.dynamic_php_references,
            skipped_html_references: after.skipped_html_references - self.skipped_html_references,
            unsupported_hack_files: after.unsupported_hack_files - self.unsupported_hack_files,
        }
    }

    fn apply(self, stats: &mut Stats) {
        stats.files += self.files;
        stats.skipped_sql_data_statements += self.skipped_sql_data_statements;
        stats.ignored_sql_statements += self.ignored_sql_statements;
        stats.sql_statement_only += self.sql_statement_only;
        stats.dynamic_php_references += self.dynamic_php_references;
        stats.skipped_html_references += self.skipped_html_references;
        stats.unsupported_hack_files += self.unsupported_hack_files;
    }
}

pub(crate) enum Snapshot {
    Buffered {
        bytes: Vec<u8>,
        digest: [u8; 32],
    },
    Streamed {
        file: tempfile::NamedTempFile,
        digest: [u8; 32],
    },
}

impl Snapshot {
    /// Hash precisely the version that will be parsed. SQL is copied to disk
    /// while hashing rather than buffered or reopened from a changing source.
    pub fn read(path: &Path, language: &str, limit: usize, workspace: &Path) -> Result<Self> {
        if language != "sql" {
            let bytes = crate::read_limited(path, limit)?;
            let digest = Sha256::digest(&bytes).into();
            return Ok(Self::Buffered { bytes, digest });
        }
        let mut source = File::open(path)?;
        let mut file = tempfile::NamedTempFile::new_in(workspace)?;
        let mut digest = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let len = source.read(&mut buffer)?;
            if len == 0 {
                break;
            }
            digest.update(&buffer[..len]);
            file.write_all(&buffer[..len])?;
        }
        file.flush()?;
        Ok(Self::Streamed {
            file,
            digest: digest.finalize().into(),
        })
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Buffered { bytes, .. } => Some(bytes),
            Self::Streamed { .. } => None,
        }
    }

    pub fn sql_path(&self) -> Option<&Path> {
        match self {
            Self::Streamed { file, .. } => Some(file.path()),
            Self::Buffered { .. } => None,
        }
    }

    fn digest(&self) -> &[u8; 32] {
        match self {
            Self::Buffered { digest, .. } | Self::Streamed { digest, .. } => digest,
        }
    }
}

fn fingerprint() -> &'static [u8; 32] {
    static HASH: OnceLock<[u8; 32]> = OnceLock::new();
    HASH.get_or_init(|| {
        let mut hash = Sha256::new();
        hash.update(FORMAT.to_le_bytes());
        // Include compiled implementation and dependency/grammar inputs, so a
        // development build cannot silently reuse an older parser's facts.
        for input in [
            include_bytes!("lib.rs").as_slice(),
            include_bytes!("cache.rs").as_slice(),
            include_bytes!("scheduler.rs").as_slice(),
            include_bytes!("php.rs").as_slice(),
            include_bytes!("sql.rs").as_slice(),
            include_bytes!("sql/mysql_expressions.rs").as_slice(),
            include_bytes!("sql/mysql_programs.rs").as_slice(),
            include_bytes!("sql/mysql_modes.rs").as_slice(),
            include_bytes!("html.rs").as_slice(),
            include_bytes!("model.rs").as_slice(),
            include_bytes!("store.rs").as_slice(),
            include_bytes!("autoload.rs").as_slice(),
            include_bytes!("../Cargo.toml").as_slice(),
            include_bytes!("../Cargo.lock").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/bindings/rust/lib.rs").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/bindings/rust/build.rs").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/common/scanner.h").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/php/src/parser.c").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/php/src/scanner.c").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/php_only/src/parser.c").as_slice(),
            include_bytes!("../vendor/tree-sitter-php/php_only/src/scanner.c").as_slice(),
        ] {
            hash.update((input.len() as u64).to_le_bytes());
            hash.update(input);
        }
        hash.finalize().into()
    })
}

struct Table {
    name: &'static str,
    definition: &'static str,
    columns: &'static [&'static str],
    integer_columns: &'static [usize],
    nullable_column: Option<usize>,
}

const TABLES: &[Table] = &[
    Table {
        name: "nodes",
        definition: "id TEXT PRIMARY KEY, file TEXT NOT NULL, json TEXT NOT NULL",
        columns: &["id", "file", "json"],
        integer_columns: &[],
        nullable_column: None,
    },
    Table {
        name: "sql_files",
        definition: "file TEXT PRIMARY KEY, dialect TEXT, scope TEXT, role TEXT",
        columns: &["file", "dialect", "scope", "role"],
        integer_columns: &[],
        nullable_column: None,
    },
    Table {
        name: "sql_defs",
        definition: "file TEXT, name TEXT, id TEXT",
        columns: &["file", "name", "id"],
        integer_columns: &[],
        nullable_column: None,
    },
    Table {
        name: "symbols",
        definition: "domain TEXT, name TEXT, id TEXT, PRIMARY KEY(domain,name,id)",
        columns: &["domain", "name", "id"],
        integer_columns: &[],
        nullable_column: None,
    },
    Table {
        name: "opaque_traits",
        definition: "owner TEXT PRIMARY KEY",
        columns: &["owner"],
        integer_columns: &[],
        nullable_column: None,
    },
    Table {
        name: "methods",
        definition: "owner TEXT, name TEXT, id TEXT, PRIMARY KEY(owner,name,id)",
        columns: &["owner", "name", "id"],
        integer_columns: &[],
        nullable_column: None,
    },
    Table {
        name: "refs",
        definition: "source TEXT, domain TEXT, name TEXT, fallback TEXT, relation TEXT, file TEXT, line INTEGER, confidence TEXT NOT NULL, event INTEGER NOT NULL",
        columns: &[
            "source",
            "domain",
            "name",
            "fallback",
            "relation",
            "file",
            "line",
            "confidence",
            "event",
        ],
        integer_columns: &[6, 8],
        nullable_column: Some(3),
    },
    Table {
        name: "php_requires",
        definition: "source TEXT, file TEXT, event INTEGER, target TEXT, line INTEGER",
        columns: &["source", "file", "event", "target", "line"],
        integer_columns: &[2, 4],
        nullable_column: None,
    },
    Table {
        name: "edges",
        definition: "source TEXT, target TEXT, relation TEXT, file TEXT, line INTEGER, confidence TEXT NOT NULL, PRIMARY KEY(source,target,relation)",
        columns: &["source", "target", "relation", "file", "line", "confidence"],
        integer_columns: &[4],
        nullable_column: None,
    },
];

impl Store {
    /// Reusable disk-backed capture tables record attempted inserts, including
    /// those suppressed by an earlier file's first-wins record in the main store.
    pub(crate) fn initialize_capture(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS __gre_capture_active(active INTEGER)",
        )?;
        for table in TABLES {
            let values = table
                .columns
                .iter()
                .map(|c| format!("NEW.{c}"))
                .collect::<Vec<_>>()
                .join(",");
            let conflict = if table.name == "sql_files" {
                "REPLACE"
            } else {
                "IGNORE"
            };
            self.conn.execute_batch(&format!(
                "CREATE TEMP TABLE IF NOT EXISTS __gre_capture_{name}({definition});
                 CREATE TEMP TRIGGER IF NOT EXISTS __gre_capture_insert_{name}
                 BEFORE INSERT ON main.{name}
                 WHEN EXISTS(SELECT 1 FROM __gre_capture_active)
                 BEGIN INSERT OR {conflict} INTO __gre_capture_{name} VALUES({values}); END;",
                name = table.name,
                definition = table.definition,
            ))?;
        }
        self.conn.execute_batch(
            "CREATE TEMP TRIGGER IF NOT EXISTS __gre_capture_update_nodes AFTER UPDATE ON main.nodes
             WHEN EXISTS(SELECT 1 FROM __gre_capture_active)
             BEGIN UPDATE __gre_capture_nodes SET file=NEW.file,json=NEW.json WHERE id=NEW.id; END;"
        )?;
        Ok(())
    }

    pub(crate) fn start_capture(&self) -> Result<()> {
        for table in TABLES {
            self.conn
                .execute_batch(&format!("DELETE FROM __gre_capture_{}", table.name))?;
        }
        self.conn.execute_batch(
            "DELETE FROM __gre_capture_active; INSERT INTO __gre_capture_active VALUES(1)",
        )?;
        Ok(())
    }

    pub(crate) fn stop_capture(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM __gre_capture_active")?;
        Ok(())
    }
}

pub(crate) struct FileCache {
    conn: Connection,
    settings: Vec<u8>,
    telemetry: Telemetry,
}

impl FileCache {
    /// One connection per persistent worker, never per source file. Cache
    /// contention or an unusable directory may disable caching without changing
    /// extraction's output or making a previous cache entry authoritative.
    pub fn open(dir: &Path, options: &Options, max_bytes: usize) -> Result<Self> {
        let _writer = writer_access();
        std::fs::create_dir_all(dir).context("create file-fact cache directory")?;
        let conn = Connection::open(dir.join(DATABASE))?;
        // The process mutex handles our own workers. A separate process may
        // hold the cache briefly; wait a bounded interval before safely giving
        // up on this disposable optimization.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-2048; PRAGMA temp_store=FILE;
             CREATE TABLE IF NOT EXISTS entries(
                 key TEXT PRIMARY KEY, format INTEGER NOT NULL, file TEXT NOT NULL,
                 stats TEXT NOT NULL, row_count INTEGER NOT NULL, digest TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS facts(
                 key TEXT NOT NULL, table_id INTEGER NOT NULL, ordinal INTEGER NOT NULL,
                 payload TEXT NOT NULL, PRIMARY KEY(key,table_id,ordinal));",
        )?;
        let settings = serde_json::to_vec(&(max_bytes, &options.sql_dialect, &options.sql_scope))?;
        Ok(Self {
            conn,
            settings,
            telemetry: Telemetry::default(),
        })
    }

    pub fn telemetry(&self) -> Telemetry {
        self.telemetry
    }

    pub fn key(&self, relative: &str, snapshot: &Snapshot) -> String {
        let mut hash = Sha256::new();
        hash.update(fingerprint());
        hash.update((relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        hash.update((self.settings.len() as u64).to_le_bytes());
        hash.update(&self.settings);
        hash.update(snapshot.digest());
        format!("{:x}", hash.finalize())
    }

    pub fn restore(
        &mut self,
        key: &str,
        relative: &str,
        store: &Store,
        stats: &mut Stats,
    ) -> Result<bool> {
        let transaction = match self.conn.unchecked_transaction() {
            Ok(transaction) => transaction,
            Err(error) => {
                eprintln!("file-fact cache read failed for {relative:?}: {error}");
                self.telemetry.errors += 1;
                self.telemetry.misses += 1;
                return Ok(false);
            }
        };
        // Cache validation is read-only and occurs under one stable SQLite read
        // transaction. The disposable main store has journaling disabled, so
        // falling back after even one imported row would be unsafe.
        let validated = validate_entry(&transaction, key, relative);
        let cached_stats = match validated {
            Ok(Some(stats)) => stats,
            Ok(None) => {
                self.telemetry.misses += 1;
                return Ok(false);
            }
            Err(error) => {
                eprintln!("file-fact cache validation failed for {relative:?}: {error:#}");
                self.telemetry.errors += 1;
                self.telemetry.misses += 1;
                return Ok(false);
            }
        };
        import_entry(&transaction, key, store)
            .context("import validated file facts; previous graph.json left untouched")?;
        transaction
            .commit()
            .context("finish validated cache read")?;
        cached_stats.apply(stats);
        self.telemetry.hits += 1;
        Ok(true)
    }

    /// A failed cache write is a disposable optimization failure: parsing has
    /// already succeeded and its raw facts remain in the main workspace.
    pub fn save(&mut self, key: &str, relative: &str, store: &Store, stats: FileStats) {
        let _writer = writer_access();
        match save_entry(&mut self.conn, key, relative, store, stats) {
            Ok(()) => self.telemetry.writes += 1,
            Err(error) => {
                eprintln!("file-fact cache write failed for {relative:?}: {error:#}");
                self.telemetry.errors += 1;
            }
        }
    }
}

fn payload(row: &rusqlite::Row<'_>, width: usize) -> Result<String> {
    let mut fields = Vec::with_capacity(width);
    for column in 0..width {
        fields.push(match row.get_ref(column)? {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(value) => serde_json::Value::from(value),
            ValueRef::Text(value) => serde_json::Value::from(std::str::from_utf8(value)?),
            _ => bail!("unexpected raw fact SQLite type"),
        });
    }
    Ok(serde_json::to_string(&fields)?)
}

fn hash_row(hash: &mut Sha256, table: usize, ordinal: i64, payload: &str) {
    hash.update((table as u64).to_le_bytes());
    hash.update(ordinal.to_le_bytes());
    hash.update((payload.len() as u64).to_le_bytes());
    hash.update(payload.as_bytes());
}

fn save_entry(
    conn: &mut Connection,
    key: &str,
    relative: &str,
    store: &Store,
    stats: FileStats,
) -> Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute("DELETE FROM facts WHERE key=?1", [key])?;
    transaction.execute("DELETE FROM entries WHERE key=?1", [key])?;
    let mut digest = Sha256::new();
    let stats = serde_json::to_string(&stats)?;
    digest.update(stats.as_bytes());
    let mut count = 0_u64;
    {
        let mut insert = transaction.prepare_cached("INSERT INTO facts VALUES(?1,?2,?3,?4)")?;
        for (table_id, table) in TABLES.iter().enumerate() {
            let mut query = store.conn.prepare(&format!(
                "SELECT * FROM __gre_capture_{} ORDER BY rowid",
                table.name
            ))?;
            let mut rows = query.query([])?;
            let mut ordinal = 0_i64;
            while let Some(row) = rows.next()? {
                let payload = payload(row, table.columns.len())?;
                hash_row(&mut digest, table_id, ordinal, &payload);
                insert.execute(params![key, table_id, ordinal, payload])?;
                ordinal += 1;
                count += 1;
            }
        }
    }
    transaction.execute(
        "INSERT INTO entries VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            key,
            FORMAT,
            relative,
            stats,
            count,
            format!("{:x}", digest.finalize())
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn decode_payload(table: &Table, payload: &str) -> Result<Vec<rusqlite::types::Value>> {
    let fields: Vec<serde_json::Value> = serde_json::from_str(payload)?;
    if fields.len() != table.columns.len() {
        bail!("cache fact width mismatch");
    }
    fields
        .into_iter()
        .enumerate()
        .map(|(index, field)| {
            if table.integer_columns.contains(&index) {
                let value = field
                    .as_i64()
                    .filter(|n| *n >= 0)
                    .context("invalid cached line")?;
                Ok(rusqlite::types::Value::Integer(value))
            } else if field.is_null() && Some(index) == table.nullable_column {
                Ok(rusqlite::types::Value::Null)
            } else {
                Ok(rusqlite::types::Value::Text(
                    field.as_str().context("invalid cached text")?.to_owned(),
                ))
            }
        })
        .collect()
}

fn validate_entry(conn: &Connection, key: &str, relative: &str) -> Result<Option<FileStats>> {
    let entry = conn
        .query_row(
            "SELECT format,file,stats,row_count,digest FROM entries WHERE key=?1",
            [key],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((format, file, stats, expected_count, expected_digest)) = entry else {
        return Ok(None);
    };
    if format != FORMAT || file != relative {
        bail!("cache entry identity mismatch");
    }
    let mut digest = Sha256::new();
    digest.update(stats.as_bytes());
    let stats: FileStats = serde_json::from_str(&stats)?;
    if stats.files != 1 || stats.unsupported_hack_files > 1 {
        bail!("invalid cached counters");
    }
    let mut count = 0_u64;
    let mut query = conn.prepare(
        "SELECT table_id,ordinal,payload FROM facts WHERE key=?1 ORDER BY table_id,ordinal",
    )?;
    let mut rows = query.query([key])?;
    while let Some(row) = rows.next()? {
        let table_id: usize = row.get(0)?;
        let ordinal: i64 = row.get(1)?;
        let payload: String = row.get(2)?;
        if ordinal < 0 {
            bail!("invalid cached ordinal");
        }
        let table = TABLES.get(table_id).context("invalid cached fact table")?;
        decode_payload(table, &payload)?;
        hash_row(&mut digest, table_id, ordinal, &payload);
        count += 1;
    }
    if count != expected_count || format!("{:x}", digest.finalize()) != expected_digest {
        bail!("cached facts checksum mismatch");
    }
    Ok(Some(stats))
}

fn import_entry(conn: &Connection, key: &str, store: &Store) -> Result<()> {
    for (table_id, table) in TABLES.iter().enumerate() {
        let placeholders = (1..=table.columns.len())
            .map(|n| format!("?{n}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut insert = store.conn.prepare_cached(&format!(
            "INSERT OR IGNORE INTO {} VALUES({placeholders})",
            table.name
        ))?;
        let mut query = conn.prepare_cached(
            "SELECT payload FROM facts WHERE key=?1 AND table_id=?2 ORDER BY ordinal",
        )?;
        let mut rows = query.query(params![key, table_id])?;
        while let Some(row) = rows.next()? {
            let payload: String = row.get(0)?;
            let fields = decode_payload(table, &payload)?;
            insert.execute(rusqlite::params_from_iter(fields))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;

    #[test]
    fn save_waits_for_an_external_write_lock_longer_than_250_milliseconds() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Store::open(&workspace.path().join("main.sqlite")).unwrap();
        store.initialize_capture().unwrap();
        store.start_capture().unwrap();
        let target = Node::new("source.php", "class", "Target", "php", 2);
        store.node(&target).unwrap();
        store.stop_capture().unwrap();
        let directory = workspace.path().join("cache");
        let mut cache = FileCache::open(&directory, &Options::new(workspace.path()), 1024).unwrap();
        let database = directory.join(DATABASE);
        let (locked, acquired) = std::sync::mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            let conn = Connection::open(database).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(600));
            conn.execute_batch("COMMIT").unwrap();
        });
        acquired.recv().unwrap();
        // All parsing/capture and connection setup precede the barrier. The
        // attempted cache write therefore contends for the full old timeout.
        cache.save(
            "locked-key",
            "source.php",
            &store,
            FileStats {
                files: 1,
                ..FileStats::default()
            },
        );
        writer.join().unwrap();
        assert_eq!(cache.telemetry().writes, 1);
        assert_eq!(cache.telemetry().errors, 0);
        let restored = Store::open(&workspace.path().join("restored.sqlite")).unwrap();
        assert!(
            cache
                .restore("locked-key", "source.php", &restored, &mut Stats::default())
                .unwrap()
        );
        let (label, line): (String, String) = restored.conn.query_row("SELECT json_extract(json,'$.label'),json_extract(json,'$.source_location') FROM nodes", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(label, "Target");
        assert_eq!(line, "L2");
    }

    #[test]
    fn snapshots_hash_and_parse_the_same_source_version_after_live_source_changes() {
        let workspace = tempfile::tempdir().unwrap();
        let settings = crate::sql::Settings::new("auto", &[]).unwrap();
        for (index, language, valid, malformed, label) in [
            (
                0,
                "php",
                "<?php\nclass Original {}\n",
                "<?php\nclass Broken {",
                "Original",
            ),
            (
                1,
                "sql",
                "-- Original source\nCREATE TABLE original(id INT);\n",
                "CREATE TABLE broken (",
                "original",
            ),
        ] {
            let path = workspace.path().join(format!("source.{language}"));
            std::fs::write(&path, valid).unwrap();
            let snapshot = Snapshot::read(&path, language, 1024, workspace.path()).unwrap();
            std::fs::write(&path, malformed).unwrap();
            assert_eq!(
                snapshot.digest(),
                &<[u8; 32]>::from(Sha256::digest(valid.as_bytes()))
            );
            let newer = Snapshot::read(&path, language, 1024, workspace.path()).unwrap();
            assert_ne!(snapshot.digest(), newer.digest());
            let store =
                Store::open(&workspace.path().join(format!("facts{index}.sqlite"))).unwrap();
            let mut stats = Stats::default();
            crate::extract_contents(
                snapshot.sql_path().unwrap_or(&path),
                &format!("source.{language}"),
                language,
                snapshot.bytes(),
                &store,
                &settings,
                1024,
                &mut stats,
            )
            .unwrap();
            let (found, line): (String, String) = store.conn.query_row("SELECT json_extract(json,'$.label'),json_extract(json,'$.source_location') FROM nodes WHERE json_extract(json,'$.type')!='file' LIMIT 1", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
            assert_eq!(found, label);
            assert_eq!(line, "L2");
            assert_eq!(store.conn.query_row("SELECT count(*) FROM nodes WHERE json_extract(json,'$.label') LIKE '%Broken%' OR json_extract(json,'$.label') LIKE '%broken%'", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        }
    }

    #[test]
    fn capture_retains_attempts_suppressed_by_an_earlier_files_first_wins_rows() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Store::open(&workspace.path().join("main.sqlite")).unwrap();
        store.initialize_capture().unwrap();
        let earlier = Node::new("earlier.php", "class", "Shared", "php", 2);
        store.node(&earlier).unwrap();
        store
            .edge(&earlier.id, "target", "calls", "earlier.php", 3)
            .unwrap();
        let mut attempted = earlier.clone();
        attempted.label = "Captured".into();
        attempted.source_file = "later.php".into();
        store.start_capture().unwrap();
        store.node(&attempted).unwrap();
        store
            .edge(&attempted.id, "target", "calls", "later.php", 9)
            .unwrap();
        store.stop_capture().unwrap();
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT json_extract(json,'$.label') FROM nodes",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "Shared"
        );
        let mut cache = FileCache::open(
            &workspace.path().join("cache"),
            &Options::new(workspace.path()),
            1024,
        )
        .unwrap();
        cache.save(
            "test-key",
            "later.php",
            &store,
            FileStats {
                files: 1,
                ..FileStats::default()
            },
        );
        assert_eq!(cache.telemetry().writes, 1);
        let restored = Store::open(&workspace.path().join("restored.sqlite")).unwrap();
        assert!(
            cache
                .restore("test-key", "later.php", &restored, &mut Stats::default())
                .unwrap()
        );
        assert_eq!(
            restored
                .conn
                .query_row(
                    "SELECT json_extract(json,'$.label') FROM nodes",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "Captured"
        );
        let (file, line): (String, u64) = restored
            .conn
            .query_row("SELECT file,line FROM edges", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(file, "later.php");
        assert_eq!(line, 9);
    }
}
