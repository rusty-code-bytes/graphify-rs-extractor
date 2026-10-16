use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: impl AsRef<[u8]>) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, source).unwrap();
}

fn published(root: &Path) -> Vec<u8> {
    fs::read(root.join("graphify-out/graph.json")).unwrap()
}

fn node<'a>(graph: &'a Value, kind: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == kind && node["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label}"))
}

#[test]
fn skewed_input_and_reused_workers_preserve_graph_provenance_and_targets() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "target.php",
        "<?php\nnamespace Good;\nclass Target { static function run() {} }\n",
    );
    put(
        root,
        "decoy.php",
        "<?php\nnamespace Other;\nclass Target { static function run() {} }\n",
    );
    put(
        root,
        "caller.php",
        "<?php\nuse Good\\Target as Alias;\nfunction exercise() {\n    Alias::run();\n    absent();\n}\n",
    );
    // Both count-bound and byte-bound waves are needed. Large comments create
    // skewed source sizes without introducing deep or expensive syntax trees.
    for i in 0..70 {
        put(
            root,
            &format!("small{i:03}.php"),
            format!("<?php function f{i}() {{ f{}(); }}", (i + 1) % 70),
        );
    }
    for i in 0..3 {
        put(
            root,
            &format!("large{i}.php"),
            format!(
                "<?php function large{i}() {{ \\Good\\Target::run(); }} /*{}*/",
                " ".repeat(700 * 1024)
            ),
        );
    }
    put(
        root,
        "mysql/schema.sql",
        "CREATE TABLE parent(id INT PRIMARY KEY);\nCREATE TABLE child(pid INT REFERENCES parent(id));\n",
    );
    put(root, "index.html", "<a href='caller.php'>call</a>");
    put(
        root,
        "unsupported.php",
        "<?hh\nfunction unsupported(): void {}\n",
    );
    let mut options = Options::new(root);
    extract(&options).unwrap();
    let expected = published(root);
    let graph: Value = serde_json::from_slice(&expected).unwrap();
    let caller = node(&graph, "function", "exercise");
    let target = node(&graph, "method", "Good\\Target::run");
    let decoy = node(&graph, "method", "Other\\Target::run");
    let edges = graph["links"].as_array().unwrap();
    assert!(edges.iter().any(|edge| edge["source"] == caller["id"]
        && edge["target"] == target["id"]
        && edge["relation"] == "calls"
        && edge["source_location"] == "L4"));
    assert!(
        !edges
            .iter()
            .any(|edge| edge["source"] == caller["id"] && edge["target"] == decoy["id"])
    );
    assert_eq!(node(&graph, "file", "unsupported.php")["language"], "hack");
    for workers in [2, 4, 8] {
        options.workers = workers;
        for budget in [1, 64] {
            options.parse_input_mib = budget;
            extract(&options).unwrap();
            assert_eq!(
                published(root),
                expected,
                "workers={workers}, input MiB={budget}"
            );
        }
    }
}

#[test]
fn invalid_admission_and_worker_failures_leave_previous_graph_intact() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for i in 0..100 {
        put(
            root,
            &format!("f{i:03}.php"),
            format!("<?php function f{i}() {{}}"),
        );
    }
    let mut options = Options::new(root);
    options.workers = 4;
    options.parse_input_mib = 1;
    extract(&options).unwrap();
    let expected = published(root);
    for workers in [1, 4] {
        options.workers = workers;
        for budget in [0, usize::MAX] {
            options.parse_input_mib = budget;
            let error = extract(&options).unwrap_err().to_string();
            assert!(error.contains("parse-input-mib"), "{error}");
            assert_eq!(published(root), expected);
        }
    }
    options.parse_input_mib = 1;
    options.workers = 4;
    put(root, "malformed.php", "<?php function {");
    let error = format!("{:#}", extract(&options).unwrap_err());
    assert!(error.contains("malformed.php"), "{error}");
    assert!(error.contains("refusing partial extraction"), "{error}");
    assert_eq!(published(root), expected);
    assert_eq!(fs::read_dir(root.join("graphify-out")).unwrap().count(), 1);
    fs::remove_file(root.join("malformed.php")).unwrap();
    put(
        root,
        "oversized.php",
        format!("<?php /*{}*/", " ".repeat(2 * 1024 * 1024)),
    );
    options.max_file_mib = 1;
    assert!(extract(&options).is_err());
    assert_eq!(published(root), expected);
    fs::remove_file(root.join("oversized.php")).unwrap();
    extract(&options).unwrap();
    assert_eq!(published(root), expected);
}
