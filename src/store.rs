use crate::model::{Edge, Node};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

#[derive(Clone)]
enum Binding {
    Missing,
    Found(String),
    Ambiguous,
}

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            // This database is a disposable full-build workspace, never a published
            // cache. Every error discards it; atomic graph publication is separate.
            // Crash recovery and per-shard fsync provide no durability benefit here.
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
            PRAGMA cache_size=-8192; PRAGMA temp_store=FILE;
            CREATE TABLE nodes(id TEXT PRIMARY KEY, file TEXT NOT NULL, json TEXT NOT NULL);
            CREATE TABLE sql_files(file TEXT PRIMARY KEY, dialect TEXT, scope TEXT, role TEXT);
            CREATE TABLE sql_defs(file TEXT, name TEXT, id TEXT);
            CREATE TABLE symbols(domain TEXT, name TEXT, id TEXT, PRIMARY KEY(domain,name,id));
            CREATE TABLE opaque_traits(owner TEXT PRIMARY KEY);
            CREATE TABLE methods(owner TEXT, name TEXT, id TEXT, PRIMARY KEY(owner,name,id));
            CREATE TABLE refs(source TEXT, domain TEXT, name TEXT, fallback TEXT, relation TEXT,
                              file TEXT, line INTEGER, confidence TEXT NOT NULL);
            CREATE TABLE edges(source TEXT, target TEXT, relation TEXT, file TEXT, line INTEGER, confidence TEXT NOT NULL,
                               PRIMARY KEY(source,target,relation));
            ",
        )?;
        Ok(Self { conn })
    }
    /// Merge raw facts, preserving input order and first-wins records. Resolution
    /// happens only once, after all shards are imported into the global store.
    pub fn merge(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "ATTACH DATABASE ?1 AS shard",
            [path.to_str().context("non-UTF-8 shard path")?],
        )?;
        self.begin()?;
        for table in [
            "nodes",
            "sql_files",
            "sql_defs",
            "symbols",
            "opaque_traits",
            "methods",
            "refs",
            "edges",
        ] {
            self.conn.execute_batch(&format!(
                "INSERT OR IGNORE INTO main.{table} SELECT * FROM shard.{table} ORDER BY rowid"
            ))?;
        }
        self.commit()?;
        self.conn.execute_batch("DETACH DATABASE shard")?;
        Ok(())
    }
    /// Reuse a worker database without carrying facts into the next shard.
    pub fn clear_facts(&self) -> Result<()> {
        // Persistent shards accumulate their largest batches' page working sets.
        // Keep their caches smaller than the global resolver's 8 MiB cache.
        self.conn.execute_batch("PRAGMA cache_size=-1024")?;
        self.begin()?;
        for table in [
            "nodes",
            "sql_files",
            "sql_defs",
            "symbols",
            "opaque_traits",
            "methods",
            "refs",
            "edges",
        ] {
            self.conn.execute_batch(&format!("DELETE FROM {table}"))?;
        }
        self.commit()?;
        Ok(())
    }
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }
    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
    pub fn node(&self, node: &Node) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO nodes VALUES(?1,?2,?3)")?
            .execute(params![
                node.id,
                node.source_file,
                serde_json::to_string(node)?
            ])?;
        Ok(())
    }
    pub fn symbol(&self, domain: &str, name: &str, id: &str) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO symbols VALUES(?1,?2,?3)")?
            .execute(params![domain, name, id])?;
        Ok(())
    }
    pub fn sql_file(
        &self,
        file: &str,
        id: &str,
        dialect: &str,
        scope: &str,
        reason: &str,
        role: &str,
    ) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO sql_files VALUES(?1,?2,?3,?4)")?
            .execute(params![file, dialect, scope, role])?;
        self.conn.prepare_cached("UPDATE nodes SET json=json_set(json,'$.sql_dialect',?2,'$.sql_scope',?3,'$.sql_scope_reason',?4,'$.sql_role',?5) WHERE id=?1")?
            .execute(params![id,dialect,scope,reason,role])?;
        Ok(())
    }
    pub fn sql_definition(&self, file: &str, name: &str, id: &str) -> Result<()> {
        // Count declarations, not just IDs: repeated CREATEs in one file are ambiguous too.
        self.conn
            .prepare_cached("INSERT INTO sql_defs VALUES(?1,?2,?3)")?
            .execute(params![file, name, id])?;
        Ok(())
    }
    pub fn edge(
        &self,
        source: &str,
        target: &str,
        relation: &str,
        file: &str,
        line: usize,
    ) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO edges VALUES(?1,?2,?3,?4,?5,'EXTRACTED')")?
            .execute(params![source, target, relation, file, line])?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)] // One complete reference fact, written directly to SQLite.
    pub fn reference(
        &self,
        source: &str,
        domain: &str,
        name: &str,
        fallback: Option<&str>,
        relation: &str,
        file: &str,
        line: usize,
    ) -> Result<()> {
        let (domain, confidence) = if domain == "php-method-inferred" {
            ("php-method", "INFERRED")
        } else {
            (domain, "EXTRACTED")
        };
        self.conn
            .prepare_cached("INSERT INTO refs VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?
            .execute(params![
                source, domain, name, fallback, relation, file, line, confidence
            ])?;
        Ok(())
    }
    pub fn opaque_traits(&self, owner: &str) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO opaque_traits VALUES(?1)")?
            .execute([owner])?;
        Ok(())
    }
    pub fn method(&self, owner: &str, name: &str, id: &str) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO methods VALUES(?1,?2,?3)")?
            .execute(params![owner, name.to_ascii_lowercase(), id])?;
        Ok(())
    }
    pub fn load_autoload(&self, root: &Path, max_bytes: usize) -> Result<()> {
        crate::autoload::load(&self.conn, root, max_bytes)
    }
    fn class_target(&self, name: &str, file: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT CASE WHEN l.count IS NULL THEN u.id WHEN l.count=1 THEN l.id ELSE NULL END
             FROM (SELECT 1) LEFT JOIN local_symbols l
               ON l.domain='php-class' AND l.name=?1 AND l.file=?2
             LEFT JOIN php_file_contexts f ON f.file=?2
             LEFT JOIN class_bindings u ON u.context=f.context AND u.name=?1",
            )?
            .query_row(params![name, file], |r| r.get(0))?)
    }
    fn parent_target(&self, owner: &str) -> Result<Option<String>> {
        // parent:: is lexical: never start lookup at the child's own methods or
        // traits, and never reinterpret a trait's lexical owner as its consumer.
        let valid: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND json_extract(json,'$.type') IN ('class','anonymous_class'))
             AND NOT EXISTS(SELECT 1 FROM refs r LEFT JOIN resolved_refs x ON x.ref=r.rowid
                            WHERE r.source=?1 AND r.relation='extends' AND x.ref IS NULL)",
            [owner], |r| r.get(0))?;
        if !valid {
            return Ok(None);
        }
        let mut query = self.conn.prepare_cached(
            "SELECT target FROM edges WHERE source=?1 AND relation='extends' LIMIT 2",
        )?;
        let parents = query
            .query_map([owner], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((parents.len() == 1).then(|| parents[0].clone()))
    }
    fn method_target(
        &self,
        owner: &str,
        name: &str,
        visiting: &mut HashSet<String>,
    ) -> Result<Binding> {
        if visiting.len() >= 64 || !visiting.insert(owner.to_owned()) {
            return Ok(Binding::Ambiguous);
        }
        let result = (|| {
            let mut query = self
                .conn
                .prepare_cached("SELECT id FROM methods WHERE owner=?1 AND name=?2 LIMIT 2")?;
            let local = query
                .query_map(params![owner, name], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if local.len() == 1 {
                return Ok(Binding::Found(local[0].clone()));
            }
            if local.len() > 1 {
                return Ok(Binding::Ambiguous);
            }
            if self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM opaque_traits WHERE owner=?1)",
                [owner],
                |r| r.get::<_, bool>(0),
            )? {
                return Ok(Binding::Ambiguous);
            }
            // PHP precedence: own methods, then traits, then parent class.
            for relation in ["uses_trait", "extends"] {
                let missing: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM refs r LEFT JOIN resolved_refs x ON x.ref=r.rowid
                     WHERE r.source=?1 AND r.relation=?2 AND x.ref IS NULL)",
                    params![owner, relation],
                    |r| r.get(0),
                )?;
                if missing {
                    return Ok(Binding::Ambiguous);
                }
                let mut query = self
                    .conn
                    .prepare_cached("SELECT target FROM edges WHERE source=?1 AND relation=?2")?;
                let parents = query
                    .query_map(params![owner, relation], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut targets = HashSet::new();
                for parent in parents {
                    match self.method_target(&parent, name, visiting)? {
                        Binding::Found(id) => {
                            targets.insert(id);
                        }
                        Binding::Ambiguous => return Ok(Binding::Ambiguous),
                        Binding::Missing => (),
                    }
                }
                if targets.len() > 1 {
                    return Ok(Binding::Ambiguous);
                }
                if let Some(id) = targets.into_iter().next() {
                    return Ok(Binding::Found(id));
                }
            }
            Ok(Binding::Missing)
        })();
        visiting.remove(owner);
        result
    }
    /// Statement-level diagnostics stay opt-in: normal extraction executes the
    /// original batch and does not prepare query plans or retain SQL strings.
    fn resolver_batch(&self, sql: &str) -> Result<()> {
        if std::env::var_os("GRE_PROFILE_SQL").is_none() {
            self.conn.execute_batch(sql)?;
            return Ok(());
        }
        use rusqlite::fallible_iterator::FallibleIterator;
        let mut batch = rusqlite::Batch::new(&self.conn, sql);
        while let Some(mut statement) = batch.next()? {
            let sql = statement.expanded_sql().unwrap_or_default();
            let label = sql
                .lines()
                .filter(|line| !line.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .take(6)
                .collect::<Vec<_>>()
                .join(" ");
            let mut explain = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let plan = explain
                .query_map([], |row| row.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            eprintln!(
                "GRE_PROFILE_SQL {}",
                serde_json::json!({"statement": label, "plan": plan})
            );
            let _span = crate::profile::span(|| format!("resolve.sql:{label}"));
            statement.execute([])?;
        }
        Ok(())
    }
    pub fn resolve(&self) -> Result<u64> {
        let indexes_span = crate::profile::span(|| "resolve.indexes".into());
        // Extraction only writes facts. Build read indexes once after all imports,
        // instead of maintaining them in every worker and again during merging.
        self.resolver_batch(
            "CREATE INDEX sql_defs_lookup ON sql_defs(file,name);
            CREATE INDEX refs_lookup ON refs(domain,name);
            CREATE INDEX refs_owner ON refs(source,relation);
            CREATE INDEX nodes_file ON nodes(file);",
        )?;
        drop(indexes_span);
        let setup_span = crate::profile::span(|| "resolve.setup".into());
        self.resolver_batch(
            "CREATE TABLE unique_symbols AS
             SELECT domain,name,MIN(id) AS id FROM symbols GROUP BY domain,name HAVING COUNT(*)=1;
             CREATE UNIQUE INDEX unique_symbols_lookup ON unique_symbols(domain,name);
             CREATE TABLE local_symbols AS
             SELECT s.domain,s.name,n.file,MIN(s.id) AS id,COUNT(*) AS count
             FROM symbols s JOIN nodes n ON n.id=s.id WHERE s.domain LIKE 'php-%'
             GROUP BY s.domain,s.name,n.file;
             CREATE UNIQUE INDEX local_symbols_lookup ON local_symbols(domain,name,file);
             CREATE TABLE php_candidates AS
             SELECT f.scope,s.domain,s.name,s.id,n.file,json_extract(n.json,'$.label') AS label
             FROM symbols s JOIN nodes n ON n.id=s.id JOIN php_file_contexts f ON f.file=n.file
             WHERE s.domain IN ('php-class','php-function');
             CREATE INDEX php_candidates_lookup ON php_candidates(scope,domain,name,file);
             CREATE INDEX php_candidates_file ON php_candidates(file,name,domain);
             -- Composer path checks have a domain equality and directory range.
             -- A domain-only automatic index scans every class for every map.
             CREATE INDEX php_candidates_paths ON php_candidates(domain,file);
             CREATE TABLE php_unique_symbols AS
             SELECT scope,domain,name,MIN(id) AS id,COUNT(*) AS count FROM php_candidates GROUP BY scope,domain,name;
             CREATE UNIQUE INDEX php_unique_lookup ON php_unique_symbols(scope,domain,name);
             CREATE TABLE classmap_targets AS
             SELECT m.context,m.name,MIN(p.id) AS id,COUNT(p.id) AS count,m.present,
                    EXISTS(SELECT 1 FROM nodes n WHERE n.file=m.file) AS indexed
             FROM autoload_classmap m LEFT JOIN php_candidates p
              ON p.domain='php-class' AND p.name=m.name AND p.file=m.file
             GROUP BY m.context,m.name;
             CREATE UNIQUE INDEX classmap_targets_lookup ON classmap_targets(context,name);
             -- Keep each map outside its indexed directory-range lookup. Without
             -- this join order SQLite can choose a domain scan before the map.
             CREATE TABLE psr4_matches AS
             SELECT m.context,p.name,p.id,length(m.prefix) AS specificity,m.priority
             FROM autoload_psr4 m CROSS JOIN php_candidates p ON substr(p.name,1,length(m.prefix))=m.prefix
              AND p.file>=CASE WHEN m.dir='' THEN '' ELSE m.dir||'/' END
              AND p.file<CASE WHEN m.dir='' THEN char(1114111) ELSE m.dir||'0' END
              AND p.file=CASE WHEN m.dir='' THEN '' ELSE m.dir||'/' END||replace(replace(substr(p.label,length(m.prefix)+1),char(61696)||char(61696),char(61696)),char(92),'/')||'.php'
             WHERE p.domain='php-class'
              AND instr(replace(p.label,char(61696)||char(61696),''),char(61696))=0;
             CREATE INDEX psr4_matches_lookup ON psr4_matches(context,name,specificity,priority);
             CREATE TABLE psr4_targets AS
             SELECT x.context,x.name,MIN(x.id) AS id,COUNT(DISTINCT x.id) AS count
             FROM psr4_matches x WHERE NOT EXISTS(SELECT 1 FROM psr4_matches y
              WHERE y.context=x.context AND y.name=x.name
               AND (y.specificity>x.specificity OR (y.specificity=x.specificity AND y.priority<x.priority)))
             GROUP BY x.context,x.name;
             CREATE UNIQUE INDEX psr4_targets_lookup ON psr4_targets(context,name);
             CREATE TABLE php_psr0_paths AS
             WITH RECURSIVE parts(id,prefix,tail) AS (
              SELECT DISTINCT id,'',replace(label,char(61696)||char(61696),char(61696)) FROM php_candidates
               WHERE domain='php-class' AND instr(replace(label,char(61696)||char(61696),''),char(61696))=0
              UNION ALL SELECT id,prefix||substr(tail,1,instr(tail,char(92))),substr(tail,instr(tail,char(92))+1)
               FROM parts WHERE instr(tail,char(92))>0)
             SELECT id,replace(prefix,char(92),'/')||replace(tail,'_','/')||'.php' AS path FROM parts WHERE instr(tail,char(92))=0;
             CREATE UNIQUE INDEX php_psr0_paths_lookup ON php_psr0_paths(id);
             CREATE TABLE psr0_matches AS
             SELECT m.context,p.name,p.id,m.priority,(length(m.prefix)>0) AS prefixed FROM autoload_psr0 m CROSS JOIN php_candidates p
              ON substr(p.name,1,length(m.prefix))=m.prefix
              AND p.file>=CASE WHEN m.dir='' THEN '' ELSE m.dir||'/' END
              AND p.file<CASE WHEN m.dir='' THEN char(1114111) ELSE m.dir||'0' END
             JOIN php_psr0_paths path ON path.id=p.id
              AND p.file=CASE WHEN m.dir='' THEN '' ELSE m.dir||'/' END||path.path
             WHERE p.domain='php-class';
             CREATE INDEX psr0_matches_lookup ON psr0_matches(context,name,prefixed,priority);
             CREATE TABLE psr0_targets AS
             SELECT x.context,x.name,MIN(x.id) AS id,COUNT(DISTINCT x.id) AS count FROM psr0_matches x
             WHERE NOT EXISTS(SELECT 1 FROM psr0_matches y WHERE y.context=x.context AND y.name=x.name
              AND (y.prefixed>x.prefixed OR (y.prefixed=x.prefixed AND y.priority<x.priority)))
             GROUP BY x.context,x.name;
             CREATE UNIQUE INDEX psr0_targets_lookup ON psr0_targets(context,name);
             CREATE TABLE psr_targets AS SELECT * FROM psr4_targets UNION ALL SELECT x.* FROM psr0_targets x
              WHERE NOT EXISTS(SELECT 1 FROM psr4_targets y WHERE x.context=y.context AND x.name=y.name);
             CREATE UNIQUE INDEX psr_targets_lookup ON psr_targets(context,name);
             CREATE TABLE class_names AS
             SELECT DISTINCT f.context,CASE WHEN r.domain='php-method' THEN substr(r.name,1,instr(r.name,'::')-1) ELSE r.name END AS name
             FROM refs r JOIN php_file_contexts f ON f.file=r.file WHERE r.domain IN ('php-class','php-method');
             CREATE TABLE class_contexts AS
             SELECT names.context,names.name,c.context AS map_context,c.authoritative,length(c.context) AS specificity
             FROM class_names names JOIN composer_contexts own ON own.context=names.context
             JOIN composer_contexts c ON c.scope=own.scope AND (c.context='' OR c.context=names.context
              OR substr(names.context,1,length(c.context)+1)=c.context||'/');
             CREATE INDEX class_contexts_lookup ON class_contexts(context,name,specificity);
             CREATE TABLE class_map_contexts AS
             SELECT names.context,names.name,
              (SELECT x.map_context FROM class_contexts x
               LEFT JOIN classmap_targets m ON m.context=x.map_context AND m.name=x.name
               LEFT JOIN psr_targets p ON p.context=x.map_context AND p.name=x.name
               WHERE x.context=names.context AND x.name=names.name
                AND ((m.name IS NOT NULL AND (m.count>0 OR m.indexed OR NOT m.present))
                 OR (p.name IS NOT NULL AND (x.authoritative=0 OR m.name IS NOT NULL)))
               ORDER BY x.specificity DESC LIMIT 1) AS map_context,
              COALESCE((SELECT MAX(x.authoritative) FROM class_contexts x
               WHERE x.context=names.context AND x.name=names.name),0) AS authoritative,
              EXISTS(SELECT 1 FROM class_contexts x JOIN classmap_targets m ON m.context=x.map_context AND m.name=x.name
               WHERE x.context=names.context AND x.name=names.name AND m.present AND NOT m.indexed AND m.count=0) AS excluded_target
             FROM class_names names;
             CREATE UNIQUE INDEX class_map_contexts_lookup ON class_map_contexts(context,name);
             CREATE TABLE class_bindings AS
             SELECT names.context,names.name,CASE
              WHEN m.count>0 THEN CASE WHEN m.count=1 THEN m.id END
              WHEN m.name IS NOT NULL AND (m.indexed OR NOT m.present) THEN NULL
              WHEN inherited.authoritative=1 AND inherited.map_context IS NULL AND NOT inherited.excluded_target THEN NULL
              WHEN p.count=1 THEN p.id WHEN p.count>1 THEN NULL
              WHEN u.count=1 THEN u.id END AS id
             FROM class_names names LEFT JOIN composer_contexts c ON c.context=names.context
             LEFT JOIN php_unique_symbols u ON u.scope=COALESCE(c.scope,'@none') AND u.domain='php-class' AND u.name=names.name
             LEFT JOIN class_map_contexts inherited ON inherited.context=names.context AND inherited.name=names.name
             LEFT JOIN classmap_targets m ON m.context=inherited.map_context AND m.name=names.name
             LEFT JOIN psr_targets p ON p.context=inherited.map_context AND p.name=names.name;
             CREATE UNIQUE INDEX class_bindings_lookup ON class_bindings(context,name);
             CREATE TABLE autoload_function_targets AS
             SELECT f.context,p.name,f.generated,MIN(CASE WHEN f.safe THEN p.id END) AS id,
              COUNT(DISTINCT CASE WHEN f.safe THEN p.id END) AS count,MAX(NOT f.safe) AS unsafe FROM autoload_files f
             JOIN php_candidates p ON p.file=f.file AND p.domain='php-function' GROUP BY f.context,p.name,f.generated;
             CREATE UNIQUE INDEX autoload_function_targets_lookup ON autoload_function_targets(context,name,generated);
             CREATE TABLE function_names AS
             SELECT DISTINCT f.context,r.name FROM refs r JOIN php_file_contexts f ON f.file=r.file WHERE r.domain='php-function'
             UNION SELECT f.context,r.fallback FROM refs r JOIN php_file_contexts f ON f.file=r.file WHERE r.domain='php-function' AND r.fallback IS NOT NULL;
             CREATE TABLE function_evidence AS
             SELECT names.context,names.name,t.id,t.count,t.unsafe,t.generated
             FROM function_names names JOIN composer_contexts own ON own.context=names.context
             JOIN composer_contexts c ON c.scope=own.scope AND (c.context='' OR c.context=names.context OR substr(names.context,1,length(c.context)+1)=c.context||'/')
             JOIN autoload_function_targets t ON t.context=c.context AND t.name=names.name;
             CREATE INDEX function_evidence_lookup ON function_evidence(context,name,generated);
             -- Generated loader inventories outrank a bare package manifest.
             -- Rank before safety checks so an unsafe generated wrapper blocks weaker evidence.
             CREATE TABLE function_bindings AS
             SELECT names.context,names.name,CASE
              WHEN MAX(t.unsafe)=1 OR MAX(t.count)>1 OR COUNT(DISTINCT t.id)>1 THEN NULL
              WHEN COUNT(DISTINCT t.id)=1 THEN MIN(t.id) WHEN u.count=1 THEN u.id END AS id,
              (COALESCE(u.count,0)>0 OR COUNT(t.name)>0) AS exists_name
             FROM function_names names LEFT JOIN composer_contexts own ON own.context=names.context
             LEFT JOIN php_unique_symbols u ON u.scope=COALESCE(own.scope,'@none') AND u.domain='php-function' AND u.name=names.name
             LEFT JOIN function_evidence t ON t.context=names.context AND t.name=names.name
              AND t.generated=(SELECT MAX(x.generated) FROM function_evidence x WHERE x.context=names.context AND x.name=names.name)
             GROUP BY names.context,names.name;
             CREATE UNIQUE INDEX function_bindings_lookup ON function_bindings(context,name);
             CREATE TABLE resolved_refs(ref INTEGER PRIMARY KEY, target TEXT);
             INSERT INTO resolved_refs
             SELECT r.rowid, CASE WHEN l.count IS NOT NULL THEN l.id ELSE u.id END
             FROM refs r LEFT JOIN local_symbols l ON l.domain=r.domain AND l.name=r.name AND l.file=r.file
             LEFT JOIN unique_symbols u ON u.domain=r.domain AND u.name=r.name
             WHERE r.domain NOT IN ('php-class','php-function','php-method','php-parent-method','sql-table') AND (l.count=1 OR (l.count IS NULL AND u.id IS NOT NULL));
             INSERT OR IGNORE INTO resolved_refs
             SELECT r.rowid, CASE WHEN l.count IS NOT NULL THEN l.id ELSE u.id END
             FROM refs r LEFT JOIN local_symbols l ON l.domain=r.domain AND l.name=r.fallback AND l.file=r.file
             LEFT JOIN unique_symbols u ON u.domain=r.domain AND u.name=r.fallback
             WHERE r.domain NOT IN ('php-class','php-function','php-method','php-parent-method','sql-table') AND (l.count=1 OR (l.count IS NULL AND u.id IS NOT NULL))
             AND NOT EXISTS(SELECT 1 FROM symbols s WHERE s.domain=r.domain AND s.name=r.name);
             INSERT INTO resolved_refs
             SELECT r.rowid,CASE WHEN l.count IS NOT NULL THEN l.id ELSE b.id END
             FROM refs r JOIN php_file_contexts f ON f.file=r.file
             LEFT JOIN local_symbols l ON l.domain='php-class' AND l.name=r.name AND l.file=r.file
             LEFT JOIN class_bindings b ON b.context=f.context AND b.name=r.name
             WHERE r.domain='php-class' AND (l.count=1 OR (l.count IS NULL AND b.id IS NOT NULL));
             INSERT INTO resolved_refs
             SELECT r.rowid,CASE WHEN l.count IS NOT NULL THEN l.id ELSE u.id END
             FROM refs r JOIN php_file_contexts f ON f.file=r.file
             LEFT JOIN local_symbols l ON l.domain='php-function' AND l.name=r.name AND l.file=r.file
             LEFT JOIN function_bindings u ON u.context=f.context AND u.name=r.name
             WHERE r.domain='php-function' AND (l.count=1 OR (l.count IS NULL AND u.id IS NOT NULL));
             INSERT OR IGNORE INTO resolved_refs
             SELECT r.rowid,CASE WHEN l.count IS NOT NULL THEN l.id ELSE u.id END
             FROM refs r JOIN php_file_contexts f ON f.file=r.file
             LEFT JOIN local_symbols l ON l.domain='php-function' AND l.name=r.fallback AND l.file=r.file
             LEFT JOIN function_bindings u ON u.context=f.context AND u.name=r.fallback
             WHERE r.domain='php-function' AND (l.count=1 OR (l.count IS NULL AND u.id IS NOT NULL))
              AND NOT EXISTS(SELECT 1 FROM function_bindings s WHERE s.context=f.context AND s.name=r.name AND s.exists_name);
             CREATE TABLE sql_local AS
             SELECT file,name,MIN(id) AS id,COUNT(*) AS count FROM sql_defs GROUP BY file,name;
             CREATE UNIQUE INDEX sql_local_lookup ON sql_local(file,name);
             CREATE TABLE sql_scoped AS
             SELECT f.dialect,f.scope,d.name,
             COALESCE(MIN(CASE WHEN f.role!='migration' THEN d.id END),MIN(d.id)) AS id,
             CASE WHEN SUM(f.role!='migration')>0 THEN SUM(f.role!='migration') ELSE COUNT(*) END AS count
             FROM sql_defs d JOIN sql_files f ON f.file=d.file GROUP BY f.dialect,f.scope,d.name;
             CREATE UNIQUE INDEX sql_scoped_lookup ON sql_scoped(dialect,scope,name);
             INSERT INTO resolved_refs
             SELECT r.rowid,CASE WHEN l.count IS NOT NULL THEN l.id ELSE g.id END
             FROM refs r JOIN sql_files f ON f.file=r.file
             LEFT JOIN sql_local l ON l.file=r.file AND l.name=r.name
             LEFT JOIN sql_scoped g ON g.dialect=f.dialect AND g.scope=f.scope AND g.name=r.name
             WHERE r.domain='sql-table' AND (l.count=1 OR (l.count IS NULL AND g.count=1));
             INSERT OR IGNORE INTO edges
             SELECT r.source,x.target,r.relation,r.file,r.line,r.confidence
             FROM refs r JOIN resolved_refs x ON x.ref=r.rowid ORDER BY r.confidence,r.line;"
        )?;
        drop(setup_span);
        let methods_span = crate::profile::span(|| "resolve.methods".into());
        let mut classes: HashMap<(String, String), Option<String>> = HashMap::new();
        let mut cache: HashMap<(String, String), Binding> = HashMap::new();
        let mut query = self
            .conn
            .prepare("SELECT rowid,name,file,domain FROM refs WHERE domain IN ('php-method','php-parent-method')")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let reference: i64 = row.get(0)?;
            let target: String = row.get(1)?;
            let file: String = row.get(2)?;
            let domain: String = row.get(3)?;
            let Some((class, method)) = target.rsplit_once("::") else {
                continue;
            };
            let parent = domain == "php-parent-method";
            let class_key = (
                if parent {
                    format!("@parent:{class}")
                } else {
                    class.to_owned()
                },
                file,
            );
            let owner = if let Some(owner) = classes.get(&class_key) {
                owner.clone()
            } else {
                let owner = if parent {
                    self.parent_target(class)?
                } else {
                    self.class_target(class, &class_key.1)?
                };
                if classes.len() >= 4096 {
                    classes.clear();
                }
                classes.insert(class_key, owner.clone());
                owner
            };
            let Some(owner) = owner else {
                continue;
            };
            let key = (owner.clone(), method.to_owned());
            let binding = if let Some(binding) = cache.get(&key) {
                binding.clone()
            } else {
                self.method_target(&owner, method, &mut HashSet::new())?
            };
            if let Binding::Found(id) = &binding {
                self.conn
                    .prepare_cached("INSERT INTO resolved_refs VALUES(?1,?2)")?
                    .execute(params![reference, id])?;
            }
            // Keep resolver memory bounded independently of project size.
            if cache.len() >= 4096 {
                cache.clear();
            }
            cache.insert(key, binding);
        }
        drop(methods_span);
        let _project_span = crate::profile::span(|| "resolve.project".into());
        self.resolver_batch(
            "INSERT OR IGNORE INTO edges
             SELECT r.source,x.target,r.relation,r.file,r.line,r.confidence
             FROM refs r JOIN resolved_refs x ON x.ref=r.rowid WHERE r.domain IN ('php-method','php-parent-method')
             ORDER BY r.confidence,r.line;
             INSERT OR IGNORE INTO edges
             SELECT a.target,f.target,'references',f.file,f.line,f.confidence
             FROM edges a JOIN edges f ON f.source=a.source
             WHERE a.relation='alters' AND f.relation='references'
             ORDER BY f.file,f.line;",
        )?;
        Ok(self.conn.query_row("SELECT COUNT(*) FROM refs r WHERE NOT EXISTS(SELECT 1 FROM resolved_refs x WHERE x.ref=r.rowid)", [], |r| r.get(0))?)
    }
    pub fn counts(&self) -> Result<(u64, u64)> {
        Ok((
            self.conn
                .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?,
            self.conn
                .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?,
        ))
    }
    pub fn inferred_edges(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE confidence='INFERRED'",
            [],
            |row| row.get(0),
        )?)
    }
    pub fn export(&self, writer: &mut impl Write, metadata: &impl serde::Serialize) -> Result<()> {
        // Graphify's NetworkX consumers use links. Only one representation is emitted.
        writer.write_all(b"{\"directed\":true,\"multigraph\":true,\"graph\":")?;
        serde_json::to_writer(&mut *writer, metadata)?;
        writer.write_all(b",\"nodes\":[")?;
        let mut query = self.conn.prepare("SELECT json FROM nodes ORDER BY id")?;
        let mut rows = query.query([])?;
        let mut first = true;
        while let Some(row) = rows.next()? {
            if !first {
                writer.write_all(b",")?;
            }
            first = false;
            writer.write_all(row.get::<_, String>(0)?.as_bytes())?;
        }
        writer.write_all(b"],\"links\":[")?;
        let mut query = self.conn.prepare(
            "SELECT e.source,e.target,e.relation,e.file,e.line,e.confidence
            FROM edges e JOIN nodes a ON a.id=e.source JOIN nodes b ON b.id=e.target
            ORDER BY e.source,e.target,e.relation",
        )?;
        let mut rows = query.query([])?;
        first = true;
        while let Some(row) = rows.next()? {
            if !first {
                writer.write_all(b",")?;
            }
            first = false;
            let source: String = row.get(0)?;
            let target: String = row.get(1)?;
            let relation: String = row.get(2)?;
            let file: String = row.get(3)?;
            let line: u64 = row.get(4)?;
            let confidence: String = row.get(5)?;
            serde_json::to_writer(
                &mut *writer,
                &Edge {
                    source: &source,
                    target: &target,
                    relation: &relation,
                    // Graphify filters on context, not relation (including filters
                    // inferred from questions such as "Who calls foo?").
                    context: match relation.as_str() {
                        "calls" | "instantiates" => Some("call"),
                        "imports" | "includes" => Some("import"),
                        _ => None,
                    },
                    confidence: &confidence,
                    confidence_score: if confidence == "EXTRACTED" { 1.0 } else { 0.7 },
                    weight: 1.0,
                    source_file: &file,
                    source_location: (line > 0).then(|| format!("L{line}")),
                    origin: "ast",
                },
            )
            .context("serialize edge")?;
        }
        writer.write_all(b"]}\n")?;
        Ok(())
    }
}
