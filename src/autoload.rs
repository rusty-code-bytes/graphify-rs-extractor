//! Conservative, source-only Composer contexts. Never include or execute PHP.
//!
//! Generated Composer arrays have a deliberately small accepted language: literal
//! strings, concatenation, __DIR__/__FILE__, dirname(), and the two generated
//! directory variables. An unsupported expression invalidates the whole map.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};
use tree_sitter::{Node, Parser};

const NO_CONTEXT: &str = "@none";

#[derive(Clone, Copy)]
enum MapKind {
    Classmap,
    Psr4,
    Psr0,
    Files,
}

pub(crate) fn load(conn: &Connection, root: &Path, max_bytes: usize) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE composer_dirs(dir TEXT PRIMARY KEY);
         CREATE TABLE composer_contexts(context TEXT PRIMARY KEY, vendor TEXT, scope TEXT, authoritative INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE php_file_contexts(file TEXT PRIMARY KEY, context TEXT NOT NULL, scope TEXT NOT NULL);
         CREATE TABLE autoload_classmap(context TEXT, name TEXT, file TEXT, present INTEGER, PRIMARY KEY(context,name));
         CREATE TABLE autoload_psr4(context TEXT, prefix TEXT, dir TEXT, priority INTEGER);
         CREATE INDEX autoload_psr4_lookup ON autoload_psr4(context,prefix);
         CREATE TABLE autoload_psr0(context TEXT, prefix TEXT, dir TEXT, priority INTEGER);
         CREATE INDEX autoload_psr0_lookup ON autoload_psr0(context,prefix);
         CREATE TABLE autoload_psr_selected(context TEXT, name TEXT, file TEXT, PRIMARY KEY(context,name,file));
         CREATE TABLE autoload_files(context TEXT, file TEXT, safe INTEGER, generated INTEGER, PRIMARY KEY(context,file));
         CREATE TABLE autoload_file_safety(file TEXT PRIMARY KEY, safe INTEGER);
         CREATE TEMP TABLE composer_map_classmap AS SELECT * FROM autoload_classmap WHERE 0;
         CREATE TEMP TABLE composer_map_psr(context TEXT, prefix TEXT, dir TEXT, priority INTEGER, kind INTEGER);
         CREATE TEMP TABLE composer_map_files AS SELECT * FROM autoload_files WHERE 0;"
    )?;
    // Directory inventory is disk backed. Each metadata file is inspected once,
    // regardless of the number of references or source files in the directory.
    let mut query = conn.prepare("SELECT DISTINCT file FROM nodes ORDER BY file")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let file: String = row.get(0)?;
        let mut directory = Path::new(&file).parent();
        while let Some(dir) = directory {
            conn.prepare_cached("INSERT OR IGNORE INTO composer_dirs VALUES(?1)")?
                .execute([dir.to_string_lossy().as_ref()])?;
            directory = dir.parent();
        }
    }
    drop(rows);
    drop(query);
    let mut query = conn.prepare("SELECT dir FROM composer_dirs ORDER BY length(dir),dir")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let dir: String = row.get(0)?;
        // A dependency's package manifest describes the containing project's
        // loader; it does not establish an independently bootstrapped project.
        let package: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM composer_contexts WHERE vendor=?1 OR substr(?1,1,length(vendor)+1)=vendor||'/')",
            [&dir], |r| r.get(0))?;
        if package {
            continue;
        }
        let manifest = root.join(&dir).join("composer.json");
        let Some(bytes) = read(&manifest, root, max_bytes) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if !json.is_object() {
            continue;
        }
        let vendor = json
            .pointer("/config/vendor-dir")
            .and_then(Value::as_str)
            .unwrap_or("vendor");
        let Some(vendor) = relative(root, &root.join(&dir).join(vendor)) else {
            continue;
        };
        let containing_project: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM composer_contexts WHERE context='' OR substr(?1,1,length(context)+1)=context||'/')",
            [&dir], |r| r.get(0))?;
        // An extraction root (including a standalone library) or an explicit
        // nested project describes a loader context. A bundled library's bare
        // manifest, even beside generated maps, proves no loader was activated.
        // Bootstrap-dependent nested loaders need an execution-order model;
        // lexical proximity must not redirect the containing project's symbols.
        if containing_project && json.get("type").and_then(Value::as_str) != Some("project") {
            continue;
        }
        let scope = dir.clone();
        conn.execute(
            "INSERT INTO composer_contexts(context,vendor,scope) VALUES(?1,?2,?3)",
            params![dir, vendor, scope],
        )?;
        let metadata = root.join(&vendor).join("composer");
        let classmap = generated_map(
            conn,
            root,
            &dir,
            &metadata.join("autoload_classmap.php"),
            max_bytes,
            MapKind::Classmap,
        )?;
        let psr4 = generated_map(
            conn,
            root,
            &dir,
            &metadata.join("autoload_psr4.php"),
            max_bytes,
            MapKind::Psr4,
        )?;
        if !psr4 {
            source_prefixes(conn, root, &dir, json.pointer("/autoload/psr-4"), true)?;
        }
        if !generated_map(
            conn,
            root,
            &dir,
            &metadata.join("autoload_namespaces.php"),
            max_bytes,
            MapKind::Psr0,
        )? {
            source_prefixes(conn, root, &dir, json.pointer("/autoload/psr-0"), false)?;
        }
        if !generated_map(
            conn,
            root,
            &dir,
            &metadata.join("autoload_files.php"),
            max_bytes,
            MapKind::Files,
        )? && let Some(files) = json
            .pointer("/autoload/files")
            .and_then(Value::as_array)
            .filter(|paths| paths.iter().all(Value::is_string))
        {
            for file in files {
                if let Some(file) = relative(root, &root.join(&dir).join(file.as_str().unwrap())) {
                    let safe = file_safety(conn, root, &file, max_bytes)?;
                    conn.prepare_cached("INSERT OR IGNORE INTO autoload_files VALUES(?1,?2,?3,0)")?
                        .execute(params![dir, file, safe])?;
                }
            }
        }
        // Authority is meaningful only with a fully parsed generated class map.
        // Dynamic PHP that merely mentions the method cannot enable this flag.
        let real = metadata.join("autoload_real.php");
        let authoritative = classmap
            && match generated_authority(root, &real, max_bytes) {
                Some(value) => value,
                None if !real.exists() => {
                    json.pointer("/config/classmap-authoritative")
                        .and_then(Value::as_bool)
                        == Some(true)
                }
                None => false,
            };
        if authoritative {
            conn.execute(
                "UPDATE composer_contexts SET authoritative=1 WHERE context=?1",
                [&dir],
            )?;
        }
    }
    drop(rows);
    drop(query);
    conn.execute_batch(&format!(
        "INSERT INTO php_file_contexts
         SELECT n.file,COALESCE((SELECT c.context FROM composer_contexts c
          WHERE c.context='' OR substr(n.file,1,length(c.context)+1)=c.context||'/'
          ORDER BY length(c.context) DESC LIMIT 1),'{NO_CONTEXT}'),
          COALESCE((SELECT c.scope FROM composer_contexts c
          WHERE c.context='' OR substr(n.file,1,length(c.context)+1)=c.context||'/'
          ORDER BY length(c.context) DESC LIMIT 1),'{NO_CONTEXT}')
         FROM nodes n GROUP BY n.file;
         DROP TABLE composer_dirs;
         DROP TABLE composer_map_classmap; DROP TABLE composer_map_psr; DROP TABLE composer_map_files;"
    ))?;
    psr_selections(conn, root)?;
    Ok(())
}

fn psr_selections(conn: &Connection, root: &Path) -> Result<()> {
    // Excluding a selected file changes graph visibility, not Composer's search
    // order. Inspect only referenced names with visible competing declarations;
    // those labels retain the case/byte spelling lost in normalized references.
    // Stream pairs from SQLite rather than expanding contexts across all symbols
    // or caching an entire dependency inventory in memory. Never parse hidden PHP.
    // Keep referenced names outside the exact (domain,name) symbol lookup.
    // Otherwise SQLite may scan every referenced name for every class symbol.
    let _span = crate::profile::span(|| "autoload.psr_selections".into());
    let sql = "WITH names AS (
          SELECT DISTINCT f.context,CASE WHEN r.domain='php-method' THEN substr(r.name,1,instr(r.name,'::')-1) ELSE r.name END AS name
          FROM refs r JOIN php_file_contexts f ON f.file=r.file
          WHERE r.domain IN ('php-class','php-method'))
         SELECT DISTINCT names.context,names.name,json_extract(n.json,'$.label') AS label
         FROM names CROSS JOIN composer_contexts c ON c.context=names.context
         CROSS JOIN symbols s ON s.domain='php-class' AND s.name=names.name
         JOIN nodes n ON n.id=s.id
         WHERE substr(names.name,1,1)<>'@'
         ORDER BY names.context,names.name,label";
    if std::env::var_os("GRE_PROFILE_SQL").is_some() {
        let mut explain = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let plan = explain
            .query_map([], |row| row.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        eprintln!(
            "GRE_PROFILE_SQL {}",
            serde_json::json!({"statement": "autoload.psr_selections", "plan": plan})
        );
    }
    let mut query = conn.prepare(sql)?;
    let mut rows = query.query([])?;
    let mut group = (String::new(), String::new(), false, false);
    while let Some(row) = rows.next()? {
        let context: String = row.get(0)?;
        let name: String = row.get(1)?;
        let label: String = row.get(2)?;
        if group.0 != context || group.1 != name {
            unknown_psr_spelling(conn, &group)?;
            group = (context.clone(), name.clone(), false, false);
        }
        let file = match psr_file(conn, root, &context, &name, &label, true)? {
            Some(file) => Some(file),
            None => psr_file(conn, root, &context, &name, &label, false)?,
        };
        let Some(file) = file else {
            group.3 = true;
            continue;
        };
        group.2 = true;
        conn.prepare_cached("INSERT OR IGNORE INTO autoload_psr_selected VALUES(?1,?2,?3)")?
            .execute(params![context, name, file])?;
    }
    unknown_psr_spelling(conn, &group)?;
    Ok(())
}

fn unknown_psr_spelling(conn: &Connection, group: &(String, String, bool, bool)) -> Result<()> {
    // Normalized references cannot disambiguate declaration-case variants.
    // A spelling with no selected file must not be discarded in favor of one
    // that happens to exist. The empty path is an unresolved alternative and
    // cannot match a source file; names with no selected path keep static lookup.
    if group.2 && group.3 {
        conn.prepare_cached("INSERT OR IGNORE INTO autoload_psr_selected VALUES(?1,?2,'')")?
            .execute(params![group.0, group.1])?;
    }
    Ok(())
}

fn psr_file(
    conn: &Connection,
    root: &Path,
    context: &str,
    name: &str,
    label: &str,
    psr4: bool,
) -> Result<Option<String>> {
    let sql = if psr4 {
        "SELECT prefix,dir FROM autoload_psr4 WHERE context=?1 AND substr(?2,1,length(prefix))=prefix ORDER BY length(prefix) DESC,priority"
    } else {
        "SELECT prefix,dir FROM autoload_psr0 WHERE context=?1 AND substr(?2,1,length(prefix))=prefix ORDER BY (length(prefix)>0) DESC,priority"
    };
    let mut query = conn.prepare_cached(sql)?;
    let mut rows = query.query(params![context, name])?;
    while let Some(row) = rows.next()? {
        let prefix: String = row.get(0)?;
        let dir: String = row.get(1)?;
        let encoded = if psr4 {
            let Some(suffix) = label.get(prefix.len()..) else {
                continue;
            };
            suffix
        } else {
            label
        };
        let Ok(decoded) = String::from_utf8(crate::php::decode_source(encoded)) else {
            continue;
        };
        let suffix = if psr4 {
            decoded.replace('\\', "/")
        } else {
            let (namespace, class) = decoded.rsplit_once('\\').unwrap_or(("", &decoded));
            let class = class.replace('_', "/");
            if namespace.is_empty() {
                class
            } else {
                format!("{}/{class}", namespace.replace('\\', "/"))
            }
        };
        let file = if dir.is_empty() {
            format!("{suffix}.php")
        } else {
            format!("{dir}/{suffix}.php")
        };
        if root.join(&file).is_file() {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn read(path: &Path, root: &Path, max: usize) -> Option<Vec<u8>> {
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(root) {
        return None;
    }
    let file = fs::File::open(canonical).ok()?;
    if file.metadata().ok()?.len() > max as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= max).then_some(bytes)
}

fn file_safety(conn: &Connection, root: &Path, file: &str, max: usize) -> Result<bool> {
    use rusqlite::OptionalExtension;
    if let Some(safe) = conn
        .query_row(
            "SELECT safe FROM autoload_file_safety WHERE file=?1",
            [file],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
    {
        return Ok(safe);
    }
    // File-level returns can bypass conditional definitions or delegate to a
    // version-specific wrapper. Abstain without evaluating runtime guards;
    // this does not claim every declaration after a return is unreachable.
    let safe = (|| -> Option<bool> {
        let bytes = read(&root.join(file), root, max)?;
        let source = crate::php::encode_source(&bytes);
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .ok()?;
        let tree = parser.parse(source.as_bytes(), None)?;
        if tree.root_node().has_error() {
            return None;
        }
        let mut cursor = tree.walk();
        loop {
            let node = cursor.node();
            if node.kind() == "return_statement" {
                return Some(false);
            }
            let declaration = matches!(
                node.kind(),
                "function_definition"
                    | "method_declaration"
                    | "anonymous_function"
                    | "arrow_function"
                    | "class_declaration"
                    | "interface_declaration"
                    | "trait_declaration"
                    | "enum_declaration"
                    | "anonymous_class"
            );
            if !declaration && cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return Some(true);
                }
            }
        }
    })()
    .unwrap_or(false);
    conn.execute(
        "INSERT INTO autoload_file_safety VALUES(?1,?2)",
        params![file, safe],
    )?;
    Ok(safe)
}

fn normalized(path: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::CurDir => (),
            other => result.push(other.as_os_str()),
        }
    }
    Some(result)
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    normalized(path)?
        .strip_prefix(root)
        .ok()?
        .to_str()
        .map(|p| p.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn source_prefixes(
    conn: &Connection,
    root: &Path,
    context: &str,
    value: Option<&Value>,
    psr4: bool,
) -> Result<()> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Ok(());
    };
    let mut seen = std::collections::HashSet::new();
    if map.keys().any(|key| !seen.insert(key.to_ascii_lowercase())) {
        return Ok(());
    }
    let mut priority = 0;
    // Composer emits prefixes in reverse lexical order. PSR-0 tries that order
    // directly; PSR-4 additionally prioritizes the longest namespace prefix.
    for (prefix, value) in map.iter().rev() {
        if psr4 && !prefix.is_empty() && !prefix.ends_with('\\') {
            continue;
        }
        let prefix = crate::php::encode_source(prefix.as_bytes());
        let paths: &[Value] = match value {
            Value::String(_) => std::slice::from_ref(value),
            Value::Array(paths) if paths.iter().all(Value::is_string) => paths,
            _ => continue,
        };
        for path in paths {
            if let Some(dir) = relative(root, &root.join(context).join(path.as_str().unwrap())) {
                let table = if psr4 {
                    "autoload_psr4"
                } else {
                    "autoload_psr0"
                };
                conn.prepare_cached(&format!("INSERT INTO {table} VALUES(?1,?2,?3,?4)"))?
                    .execute(params![context, prefix.to_ascii_lowercase(), dir, priority])?;
                priority += 1;
            }
        }
    }
    Ok(())
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn literal(raw: &str) -> Option<String> {
    // Composer's generated maps use PHP single-quoted literals. Accepting just
    // that spelling avoids interpolation and double-quote escape ambiguities.
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut value = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('\\' | '\'')) {
            value.push(chars.next()?);
        } else {
            value.push(c);
        }
    }
    Some(value)
}

fn expression(
    node: Node<'_>,
    source: &str,
    path: &Path,
    vars: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth > 64 {
        return None;
    }
    let raw = text(node, source);
    if raw == "__DIR__" {
        return path.parent()?.to_str().map(str::to_owned);
    }
    if raw == "__FILE__" {
        return path.to_str().map(str::to_owned);
    }
    match node.kind() {
        "string" => String::from_utf8(crate::php::decode_source(&literal(raw)?)).ok(),
        "variable_name" => vars.get(raw).cloned(),
        "parenthesized_expression" | "argument" => {
            expression(node.named_child(0)?, source, path, vars, depth + 1)
        }
        "binary_expression" if text(node.child_by_field_name("operator")?, source) == "." => {
            Some(format!(
                "{}{}",
                expression(
                    node.child_by_field_name("left")?,
                    source,
                    path,
                    vars,
                    depth + 1
                )?,
                expression(
                    node.child_by_field_name("right")?,
                    source,
                    path,
                    vars,
                    depth + 1
                )?
            ))
        }
        "function_call_expression"
            if text(node.child_by_field_name("function")?, source) == "dirname" =>
        {
            let args = node.child_by_field_name("arguments")?;
            if args.named_child_count() != 1 {
                return None;
            }
            let value = expression(args.named_child(0)?, source, path, vars, depth + 1)?;
            Path::new(&value).parent()?.to_str().map(str::to_owned)
        }
        _ => None,
    }
}

fn generated_map(
    conn: &Connection,
    root: &Path,
    context: &str,
    path: &Path,
    max: usize,
    kind: MapKind,
) -> Result<bool> {
    let Some(bytes) = read(path, root, max) else {
        return Ok(false);
    };
    let source = crate::php::encode_source(&bytes);
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_php::LANGUAGE_PHP.into())?;
    let Some(tree) = parser.parse(source.as_bytes(), None) else {
        return Ok(false);
    };
    if tree.root_node().has_error() {
        return Ok(false);
    }
    // journal_mode=OFF makes rollback unsuitable for discarding partial imports.
    // Stage on disk and only publish after the complete map validates.
    conn.execute_batch("DELETE FROM composer_map_classmap; DELETE FROM composer_map_psr; DELETE FROM composer_map_files;")?;
    let valid = (|| -> Result<Option<()>> {
        let mut vars = HashMap::new();
        let mut returned = None;
        for node in tree
            .root_node()
            .named_children(&mut tree.root_node().walk())
        {
            match node.kind() {
                "php_tag" | "comment" => (),
                "expression_statement" if returned.is_none() => {
                    let Some(assign) = node
                        .named_child(0)
                        .filter(|n| n.kind() == "assignment_expression")
                    else {
                        return Ok(None);
                    };
                    let Some(left) = assign.child_by_field_name("left") else {
                        return Ok(None);
                    };
                    let name = text(left, &source);
                    if !matches!(name, "$vendorDir" | "$baseDir") || vars.contains_key(name) {
                        return Ok(None);
                    }
                    let Some(value) = assign
                        .child_by_field_name("right")
                        .and_then(|n| expression(n, &source, path, &vars, 0))
                    else {
                        return Ok(None);
                    };
                    vars.insert(name.to_owned(), value);
                }
                "return_statement" if returned.is_none() => {
                    returned = node.named_child(0);
                }
                _ => return Ok(None),
            }
        }
        let Some(array) = returned.filter(|n| n.kind() == "array_creation_expression") else {
            return Ok(None);
        };
        let mut priority = 0;
        let mut seen = std::collections::HashSet::new();
        for item in array.named_children(&mut array.walk()) {
            if item.kind() == "comment" {
                continue;
            }
            if item.kind() != "array_element_initializer" || item.named_child_count() != 2 {
                return Ok(None);
            }
            let Some(key) = item.named_child(0).and_then(|n| literal(text(n, &source))) else {
                return Ok(None);
            };
            // Duplicate PHP array keys have last-wins behavior; abstain rather
            // than accidentally importing a partial first-wins class map.
            if !seen.insert(key.to_ascii_lowercase()) {
                return Ok(None);
            }
            let value = item.named_child(1).unwrap();
            if matches!(kind, MapKind::Psr4 | MapKind::Psr0) {
                if (matches!(kind, MapKind::Psr4) && !key.is_empty() && !key.ends_with('\\'))
                    || value.kind() != "array_creation_expression"
                {
                    return Ok(None);
                }
                for item in value.named_children(&mut value.walk()) {
                    if item.kind() == "comment" {
                        continue;
                    }
                    if item.kind() != "array_element_initializer" || item.named_child_count() != 1 {
                        return Ok(None);
                    }
                    let Some(dir) = item
                        .named_child(0)
                        .and_then(|n| expression(n, &source, path, &vars, 0))
                    else {
                        return Ok(None);
                    };
                    let Some(dir) = relative(root, Path::new(&dir)) else {
                        return Ok(None);
                    };
                    conn.prepare_cached("INSERT INTO composer_map_psr VALUES(?1,?2,?3,?4,?5)")?
                        .execute(params![
                            context,
                            key.to_ascii_lowercase(),
                            dir,
                            priority,
                            if matches!(kind, MapKind::Psr4) { 4 } else { 0 }
                        ])?;
                    priority += 1;
                }
            } else {
                let Some(file) = expression(value, &source, path, &vars, 0)
                    .and_then(|p| relative(root, Path::new(&p)))
                else {
                    return Ok(None);
                };
                if matches!(kind, MapKind::Files) {
                    let safe = file_safety(conn, root, &file, max)?;
                    conn.prepare_cached("INSERT INTO composer_map_files VALUES(?1,?2,?3,1)")?
                        .execute(params![context, file, safe])?;
                    continue;
                }
                let present = root
                    .join(&file)
                    .canonicalize()
                    .ok()
                    .is_some_and(|p| p.starts_with(root) && p.is_file());
                conn.prepare_cached("INSERT INTO composer_map_classmap VALUES(?1,?2,?3,?4)")?
                    .execute(params![context, key.to_ascii_lowercase(), file, present])?;
            }
        }
        Ok(Some(()))
    })();
    if matches!(valid, Ok(Some(()))) {
        conn.execute_batch(
            "INSERT INTO autoload_classmap SELECT * FROM composer_map_classmap;
                            INSERT INTO autoload_psr4 SELECT context,prefix,dir,priority FROM composer_map_psr WHERE kind=4;
                            INSERT INTO autoload_psr0 SELECT context,prefix,dir,priority FROM composer_map_psr WHERE kind=0;
                            INSERT OR IGNORE INTO autoload_files SELECT * FROM composer_map_files;",
        )?;
    }
    conn.execute_batch("DELETE FROM composer_map_classmap; DELETE FROM composer_map_psr; DELETE FROM composer_map_files")?;
    valid.map(|v| v.is_some())
}

fn generated_authority(root: &Path, path: &Path, max: usize) -> Option<bool> {
    let bytes = read(path, root, max)?;
    let source = crate::php::encode_source(&bytes);
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
        .ok()?;
    let tree = parser.parse(source.as_bytes(), None)?;
    if tree.root_node().has_error() {
        return None;
    }
    // Accept only Composer's generated initializer and the final unconditional
    // literal setting before its return. Nested/dynamic settings are unknown.
    let mut setting = None;
    let mut initializer = false;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "method_declaration"
            && node
                .child_by_field_name("name")
                .is_some_and(|n| text(n, &source) == "getLoader")
        {
            let mut parent = node.parent();
            while parent.is_some_and(|n| n.kind() != "class_declaration") {
                parent = parent.and_then(|n| n.parent());
            }
            if parent
                .and_then(|n| n.child_by_field_name("name"))
                .is_some_and(|n| text(n, &source).starts_with("ComposerAutoloaderInit"))
            {
                if initializer {
                    return None;
                }
                initializer = true;
                let body = node.child_by_field_name("body")?;
                // A conditional setter can override a preceding literal setter.
                let mut nested = body.walk();
                loop {
                    let current = nested.node();
                    if current.kind() == "member_call_expression"
                        && current
                            .child_by_field_name("object")
                            .is_some_and(|n| text(n, &source) == "$loader")
                        && current
                            .child_by_field_name("name")
                            .is_some_and(|n| text(n, &source) == "setClassMapAuthoritative")
                        && current.parent().and_then(|n| n.parent()) != Some(body)
                    {
                        return None;
                    }
                    if nested.goto_first_child() {
                        continue;
                    }
                    while !nested.goto_next_sibling() {
                        if !nested.goto_parent() || nested.node() == body {
                            break;
                        }
                    }
                    if nested.node() == body {
                        break;
                    }
                }
                for statement in body.named_children(&mut body.walk()) {
                    if statement.kind() == "return_statement" {
                        break;
                    }
                    let Some(call) = statement.named_child(0).filter(|n| {
                        statement.kind() == "expression_statement"
                            && n.kind() == "member_call_expression"
                    }) else {
                        continue;
                    };
                    if call
                        .child_by_field_name("object")
                        .is_some_and(|n| text(n, &source) == "$loader")
                        && call
                            .child_by_field_name("name")
                            .is_some_and(|n| text(n, &source) == "setClassMapAuthoritative")
                    {
                        let args = call.child_by_field_name("arguments")?;
                        if args.named_child_count() != 1 {
                            return None;
                        }
                        setting = match text(args.named_child(0)?, &source) {
                            "true" => Some(true),
                            "false" => Some(false),
                            _ => return None,
                        };
                    }
                }
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return setting;
            }
        }
    }
}
