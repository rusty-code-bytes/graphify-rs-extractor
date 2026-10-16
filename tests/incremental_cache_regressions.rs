use graphify_extractor::{Options, extract};
use rusqlite::Connection;
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

const DATABASE: &str = "raw-facts-v1.sqlite3";

fn put(root: &Path, name: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn run(options: &Options) -> Vec<u8> {
    extract(options).unwrap();
    fs::read(options.root.join("graphify-out/graph.json")).unwrap()
}

fn equivalent(options: &Options) -> Value {
    let cached = run(options);
    let mut fresh = options.clone();
    fresh.cache_dir = None;
    assert_eq!(cached, run(&fresh), "cached graph differs from fresh graph");
    serde_json::from_slice(&cached).unwrap()
}

fn node<'a>(graph: &'a Value, file: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["source_file"] == file && node["label"] == label)
        .unwrap_or_else(|| panic!("missing {file}::{label}"))
}

fn edge(graph: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    graph["links"].as_array().unwrap().iter().any(|edge| {
        edge["source"] == source["id"]
            && edge["target"] == target["id"]
            && edge["relation"] == relation
    })
}

fn cli(root: &Path, cache: &Path, workers: usize) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_graphify-extractor"))
        .arg("extract")
        .arg(root)
        .arg("--cache-dir")
        .arg(cache)
        .arg("--workers")
        .arg(workers.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    serde_json::from_str(
        stderr
            .lines()
            .find_map(|line| line.strip_prefix("GRE_CACHE "))
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn warm_hits_preserve_graph_bytes_provenance_stats_and_worker_independence() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    put(
        root,
        "base.php",
        "<?php\nclass Base { public static function save() {} }\n",
    );
    put(
        root,
        "child.php",
        "<?php\nclass Child extends Base { public static function save() {} function run() { parent::save(); } }\n",
    );
    put(
        root,
        "schema.sql",
        "CREATE TABLE items (id INT PRIMARY KEY);\nCREATE VIEW listed AS SELECT id FROM items;\nINSERT INTO items VALUES (1);\n",
    );
    put(
        root,
        "index.html",
        "<a href='base.php'>Base</a><script src='data:text/javascript,ignored'></script>",
    );
    put(
        root,
        "typed.php",
        "<?hh // Unsupported Hack remains inventoried\nclass Typed {}\n",
    );
    let fresh = run(&Options::new(root));
    let cold = cli(root, cache.path(), 1);
    assert_eq!(cold["misses"], 5);
    assert_eq!(cold["writes"], 5);
    assert_eq!(cold["errors"], 0);
    for workers in [1, 3, 2] {
        let warm = cli(root, cache.path(), workers);
        assert_eq!(warm["hits"], 5);
        assert_eq!(warm["misses"], 0);
        assert_eq!(warm["errors"], 0);
        assert_eq!(
            fs::read(root.join("graphify-out/graph.json")).unwrap(),
            fresh
        );
    }
    let graph: Value = serde_json::from_slice(&fresh).unwrap();
    let caller = node(&graph, "child.php", "Child::run");
    assert_eq!(caller["source_location"], "L2");
    assert!(edge(
        &graph,
        caller,
        node(&graph, "base.php", "Base::save"),
        "calls"
    ));
    assert!(!edge(
        &graph,
        caller,
        node(&graph, "child.php", "Child::save"),
        "calls"
    ));
    assert_eq!(graph["graph"]["stats"]["unsupported_hack_files"], 1);
    assert_eq!(node(&graph, "typed.php", "typed.php")["language"], "hack");
}

#[test]
fn shared_cache_has_complete_warm_hits_across_persistent_workers_and_waves() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    for index in 0..280 {
        put(
            root,
            &format!("classes/{index:03}.php"),
            format!(
                "<?php\nclass C{index} {{ function run() {{ new C{}(); }} }}\n",
                (index + 1) % 280
            ),
        );
    }
    let fresh = run(&Options::new(root));
    let cold = cli(root, cache.path(), 8);
    assert_eq!(cold["writes"], 280);
    assert_eq!(cold["errors"], 0);
    for workers in [8, 2, 1] {
        let warm = cli(root, cache.path(), workers);
        assert_eq!(warm["hits"], 280);
        assert_eq!(warm["misses"], 0);
        assert_eq!(warm["errors"], 0);
        assert_eq!(
            fs::read(root.join("graphify-out/graph.json")).unwrap(),
            fresh
        );
    }
}

#[test]
fn transient_external_writer_longer_than_the_old_timeout_keeps_every_cache_write() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    let source = |target: &str| {
        format!(
            "<?php\nfunction before() {{}}\nfunction after() {{}}\nfunction caller() {{ {target}(); }}\n"
        )
    };
    put(root, "source.php", source("before"));
    assert_eq!(cli(root, cache.path(), 1)["writes"], 1);
    put(root, "source.php", source("after"));
    let (locked, acquired) = std::sync::mpsc::sync_channel(0);
    let database = cache.path().join(DATABASE);
    let writer = std::thread::spawn(move || {
        let conn = Connection::open(database).unwrap();
        // Signal after SQLite grants the write lock, so the extraction cannot
        // accidentally run before contention starts.
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));
        conn.execute_batch("COMMIT").unwrap();
    });
    acquired.recv().unwrap();
    let cold = cli(root, cache.path(), 8);
    writer.join().unwrap();
    assert_eq!(cold["misses"], 1);
    assert_eq!(cold["writes"], 1);
    assert_eq!(cold["errors"], 0);
    let cached = fs::read(root.join("graphify-out/graph.json")).unwrap();
    assert_eq!(cached, run(&Options::new(root)));
    let graph: Value = serde_json::from_slice(&cached).unwrap();
    let caller = node(&graph, "source.php", "caller");
    assert_eq!(caller["source_location"], "L4");
    assert!(edge(
        &graph,
        caller,
        node(&graph, "source.php", "after"),
        "calls"
    ));
    assert!(!edge(
        &graph,
        caller,
        node(&graph, "source.php", "before"),
        "calls"
    ));
    let warm = cli(root, cache.path(), 8);
    assert_eq!(warm["hits"], 1);
    assert_eq!(warm["misses"], 0);
    assert_eq!(warm["errors"], 0);
}

#[test]
fn cached_callers_rebind_after_composer_maps_exclusions_deletions_and_upgrades() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    put(root, "composer.json", "{}");
    let autoload = |which: &str| {
        format!(
            "<?php $vendorDir=dirname(__DIR__); return array('Lib\\\\Target'=>$vendorDir.'/{which}/Target.php');"
        )
    };
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        autoload("installed"),
    );
    for base in ["vendor/installed", "bundled"] {
        put(
            root,
            &format!("{base}/Target.php"),
            "<?php\nnamespace Lib; class Target { public static function save() {} }\n",
        );
    }
    put(
        root,
        "caller.php",
        "<?php\nnamespace App; use Lib\\Target as Alias; function run() { Alias::save(); }\n",
    );
    let mut options = Options::new(root);
    options.cache_dir = Some(cache.path().into());
    options.workers = 3;
    let graph = equivalent(&options);
    let caller = node(&graph, "caller.php", "App\\run");
    assert!(edge(
        &graph,
        caller,
        node(&graph, "vendor/installed/Target.php", "Lib\\Target::save"),
        "calls"
    ));
    assert!(!edge(
        &graph,
        caller,
        node(&graph, "bundled/Target.php", "Lib\\Target::save"),
        "calls"
    ));

    options.exclude.push("vendor/**".into());
    let graph = equivalent(&options);
    assert!(!edge(
        &graph,
        node(&graph, "caller.php", "App\\run"),
        node(&graph, "bundled/Target.php", "Lib\\Target::save"),
        "calls"
    ));
    assert!(
        !graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["source_file"] == "vendor/installed/Target.php")
    );
    options.exclude.clear();
    equivalent(&options);

    put(
        root,
        "vendor/composer/autoload_classmap.php",
        "<?php $vendorDir=dirname(__DIR__); $baseDir=dirname($vendorDir); return array('Lib\\\\Target'=>$baseDir.'/bundled/Target.php');",
    );
    let graph = equivalent(&options);
    let caller = node(&graph, "caller.php", "App\\run");
    assert!(edge(
        &graph,
        caller,
        node(&graph, "bundled/Target.php", "Lib\\Target::save"),
        "calls"
    ));
    assert!(!edge(
        &graph,
        caller,
        node(&graph, "vendor/installed/Target.php", "Lib\\Target::save"),
        "calls"
    ));

    put(
        root,
        "bundled/Target.php",
        "<?php\nnamespace Lib; class Target { public static function replaced() {} }\n",
    );
    let graph = equivalent(&options);
    let caller = node(&graph, "caller.php", "App\\run");
    assert!(!edge(
        &graph,
        caller,
        node(&graph, "vendor/installed/Target.php", "Lib\\Target::save"),
        "calls"
    ));
    assert!(!edge(
        &graph,
        caller,
        node(&graph, "bundled/Target.php", "Lib\\Target::replaced"),
        "calls"
    ));
    fs::remove_file(root.join("bundled/Target.php")).unwrap();
    let graph = equivalent(&options);
    assert!(
        !graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["source_file"] == "bundled/Target.php")
    );
    assert!(!edge(
        &graph,
        node(&graph, "caller.php", "App\\run"),
        node(&graph, "vendor/installed/Target.php", "Lib\\Target::save"),
        "calls"
    ));
}

#[test]
fn cached_ambiguity_and_nested_project_context_remain_source_grounded() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    for (context, namespace, base) in [("outer", "Outer", "one"), ("outer/nested", "Inner", "two")]
    {
        put(
            root,
            &format!("{context}/composer.json"),
            format!(r#"{{"type":"project","autoload":{{"psr-4":{{"Lib\\":"{base}/"}}}}}}"#),
        );
        put(
            root,
            &format!("{context}/{base}/Target.php"),
            "<?php\nnamespace Lib; class Target { static function save() {} }\n",
        );
        put(
            root,
            &format!("{context}/caller.php"),
            format!(
                "<?php\nnamespace {namespace}; use Lib\\Target as Alias; function run() {{ Alias::save(); }}\n"
            ),
        );
    }
    put(
        root,
        "outside.php",
        "<?php\nfunction unresolved() { Lib\\Target::save(); }\n",
    );
    let mut options = Options::new(root);
    options.cache_dir = Some(cache.path().into());
    for workers in [1, 3] {
        options.workers = workers;
        let graph = equivalent(&options);
        for (context, namespace, own, wrong) in [
            (
                "outer",
                "Outer",
                "outer/one/Target.php",
                "outer/nested/two/Target.php",
            ),
            (
                "outer/nested",
                "Inner",
                "outer/nested/two/Target.php",
                "outer/one/Target.php",
            ),
        ] {
            let caller = node(
                &graph,
                &format!("{context}/caller.php"),
                &format!("{namespace}\\run"),
            );
            assert!(edge(
                &graph,
                caller,
                node(&graph, own, "Lib\\Target::save"),
                "calls"
            ));
            assert!(!edge(
                &graph,
                caller,
                node(&graph, wrong, "Lib\\Target::save"),
                "calls"
            ));
        }
        let outside = node(&graph, "outside.php", "unresolved");
        assert!(
            !graph["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["source"] == outside["id"] && e["relation"] == "calls")
        );
    }
}

#[test]
fn cache_keys_include_relative_paths_sql_settings_and_limits() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    put(root, "one/schema.sql", "CREATE TABLE items (id INT);\n");
    put(
        root,
        "two/view.sql",
        "CREATE TABLE visible (id INT REFERENCES items(id));\n",
    );
    let mut options = Options::new(root);
    options.cache_dir = Some(cache.path().into());
    equivalent(&options);
    options.sql_scope.push("**=shared".into());
    let graph = equivalent(&options);
    assert!(edge(
        &graph,
        node(&graph, "two/view.sql", "visible"),
        node(&graph, "one/schema.sql", "items"),
        "references"
    ));
    options.sql_dialect = "postgres".into();
    equivalent(&options);
    fs::rename(root.join("one/schema.sql"), root.join("one/renamed.sql")).unwrap();
    let graph = equivalent(&options);
    assert!(
        !graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["source_file"] == "one/schema.sql")
    );
    assert!(edge(
        &graph,
        node(&graph, "two/view.sql", "visible"),
        node(&graph, "one/renamed.sql", "items"),
        "references"
    ));

    put(
        root,
        "large.php",
        format!("<?php\n/*{}*/\nclass Large {{}}", "x".repeat(1024 * 1024)),
    );
    options.max_file_mib = 2;
    run(&options);
    let previous = fs::read(root.join("graphify-out/graph.json")).unwrap();
    options.max_file_mib = 1;
    assert!(extract(&options).is_err());
    assert_eq!(
        fs::read(root.join("graphify-out/graph.json")).unwrap(),
        previous
    );
}

#[test]
fn sql_capture_retains_file_metadata_updates_duplicate_definitions_and_first_provenance() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    let source = "USE one;\nCREATE TABLE users(id INT);\nALTER TABLE users DISABLE KEYS;\nCREATE TABLE child(uid INT REFERENCES users(id));\n";
    put(root, "schema.sql", source);
    put(
        root,
        "unrelated.sql",
        "USE one;\nCREATE TABLE users(id INT);\n",
    );
    let mut options = Options::new(root);
    options.cache_dir = Some(cache.path().into());
    let graph = equivalent(&options);
    let file = node(&graph, "schema.sql", "schema.sql");
    assert_eq!(file["sql_scope"], "file:schema.sql");
    assert_eq!(file["sql_scope_reason"], "dump-keys");
    let child = node(&graph, "schema.sql", "child");
    assert_eq!(child["source_location"], "L4");
    assert!(edge(
        &graph,
        child,
        node(&graph, "schema.sql", "users"),
        "references"
    ));
    assert!(!edge(
        &graph,
        child,
        node(&graph, "unrelated.sql", "users"),
        "references"
    ));
    assert_eq!(run(&options), run(&options));

    put(
        root,
        "schema.sql",
        format!("{source}CREATE TABLE users(id INT);\n"),
    );
    for _ in 0..2 {
        let graph = equivalent(&options);
        let users = node(&graph, "schema.sql", "users");
        assert_eq!(users["source_location"], "L2");
        let child = node(&graph, "schema.sql", "child");
        assert!(!edge(&graph, child, users, "references"));
        assert!(!edge(
            &graph,
            child,
            node(&graph, "unrelated.sql", "users"),
            "references"
        ));
        let defines = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["source"] == file["id"]
                    && e["target"] == users["id"]
                    && e["relation"] == "defines"
            })
            .unwrap();
        assert_eq!(defines["source_location"], "L2");
    }
}

#[test]
fn malformed_edits_never_reuse_stale_facts_or_replace_the_previous_graph() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    let mut options = Options::new(root);
    options.cache_dir = Some(cache.path().into());
    for (name, valid, malformed) in [
        (
            "source.php",
            "<?php\nclass Correct {}\n",
            "<?php\nclass Broken {\n",
        ),
        (
            "source.sql",
            "CREATE TABLE correct (id INT);\n",
            "CREATE TABLE broken (\n",
        ),
    ] {
        put(root, name, valid);
        let previous = run(&options);
        put(root, name, malformed);
        let error = extract(&options).unwrap_err();
        assert!(format!("{error:#}").contains(name));
        assert_eq!(
            fs::read(root.join("graphify-out/graph.json")).unwrap(),
            previous
        );
        put(root, name, valid);
        assert_eq!(run(&options), previous);
    }
}

#[test]
fn corrupt_rows_counts_stats_and_database_fall_back_before_importing_any_facts() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    put(
        root,
        "source.php",
        "<?php\nfunction target() {} function caller() { target(); }\n",
    );
    let mut options = Options::new(root);
    options.cache_dir = Some(cache.path().into());
    let expected = run(&options);
    for corruption in [
        "UPDATE facts SET payload='[]' WHERE table_id=6",
        "DELETE FROM facts WHERE table_id=6",
        "UPDATE entries SET stats=json_set(stats,'$.dynamic_php_references',99)",
        "UPDATE entries SET digest='corrupt'",
    ] {
        Connection::open(cache.path().join(DATABASE))
            .unwrap()
            .execute(corruption, [])
            .unwrap();
        assert_eq!(run(&options), expected);
    }
    fs::write(cache.path().join(DATABASE), b"this is not SQLite").unwrap();
    assert_eq!(run(&options), expected);
}

#[test]
fn unavailable_cache_is_optional_and_cache_directory_cannot_hide_project_root() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    put(root, "source.php", "<?php\nclass Included {}\n");
    let expected = run(&Options::new(root));
    let mut options = Options::new(root);
    options.cache_dir = Some(root.join("source.php"));
    assert_eq!(run(&options), expected);
    options.cache_dir = Some(root.into());
    assert!(format!("{:#}", extract(&options).unwrap_err()).contains("cache directory"));
    assert_eq!(
        fs::read(root.join("graphify-out/graph.json")).unwrap(),
        expected
    );
    options.cache_dir = Some(root.parent().unwrap().into());
    assert!(extract(&options).is_err());
}

#[test]
fn cache_directory_inside_project_is_excluded_and_cache_write_failure_preserves_fresh_facts() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    put(root, "source.php", "<?php\nclass Included {}\n");
    let expected = run(&Options::new(root));
    let mut options = Options::new(root);
    let dir = root.join(".raw-cache");
    options.cache_dir = Some(dir.clone());
    assert_eq!(run(&options), expected);
    put(&dir, "should-not-be-parsed.php", "<?php class Invalid {\n");
    assert_eq!(run(&options), expected);
    // Abort the cache transaction after parsing: this may not affect graph data.
    Connection::open(dir.join(DATABASE)).unwrap().execute_batch(
        "CREATE TRIGGER reject_cache_writes BEFORE INSERT ON entries BEGIN SELECT RAISE(ABORT,'test cache write failure'); END;"
    ).unwrap();
    put(root, "source.php", "<?php\nclass Changed {}\n");
    let cached = run(&options);
    fs::remove_file(dir.join("should-not-be-parsed.php")).unwrap();
    assert_eq!(cached, run(&Options::new(root)));
}
