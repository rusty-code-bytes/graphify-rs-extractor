use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;
fn put(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
#[test]
fn bundled_package_uses_containing_project_map_for_external_dependencies() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"vendor/library/"}}}"#,
    );
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"library","autoload":{"psr-4":{"Package\\":"src/"}}}"#,
    );
    put(
        root,
        "vendor/library/Service.php",
        "<?php namespace Vendor; class Service { static function run() {} }",
    );
    put(
        root,
        "bundled/Service.php",
        "<?php namespace Vendor; class Service { static function run() {} }",
    );
    put(
        root,
        "lib/package/src/Caller.php",
        "<?php namespace Package; use Vendor\\Service; function call_service() { Service::run(); }",
    );
    extract(&Options::new(root)).unwrap();
    let g: Value =
        serde_json::from_slice(&fs::read(root.join("graphify-out/graph.json")).unwrap()).unwrap();
    let ns = g["nodes"].as_array().unwrap();
    let caller = ns
        .iter()
        .find(|n| n["label"] == "Package\\call_service")
        .unwrap();
    let target = ns
        .iter()
        .find(|n| {
            n["source_file"] == "vendor/library/Service.php" && n["label"] == "Vendor\\Service::run"
        })
        .unwrap();
    let wrong = ns
        .iter()
        .find(|n| n["source_file"] == "bundled/Service.php" && n["label"] == "Vendor\\Service::run")
        .unwrap();
    let edges = g["links"].as_array().unwrap();
    assert!(edges.iter().any(|e| e["source"] == caller["id"]
        && e["target"] == target["id"]
        && e["relation"] == "calls"));
    assert!(
        !edges
            .iter()
            .any(|e| e["source"] == caller["id"] && e["target"] == wrong["id"])
    );
}

fn graph(options: &Options) -> Value {
    extract(options).unwrap();
    serde_json::from_slice(&fs::read(options.root.join("graphify-out/graph.json")).unwrap())
        .unwrap()
}

fn service(root: &Path, file: &str) {
    put(
        root,
        file,
        "<?php namespace Vendor; class Service { static function run() {} }",
    );
}

fn caller(root: &Path, file: &str) {
    put(
        root,
        file,
        "<?php namespace Package; function call_service() { \\Vendor\\Service::run(); }",
    );
}

fn calls_file(graph: &Value, source_file: &str, target_file: &str) -> bool {
    let nodes = graph["nodes"].as_array().unwrap();
    let source = nodes
        .iter()
        .find(|n| n["source_file"] == source_file && n["label"] == "Package\\call_service")
        .unwrap();
    let target = nodes
        .iter()
        .find(|n| n["source_file"] == target_file && n["label"] == "Vendor\\Service::run")
        .unwrap();
    graph["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == "calls"
    })
}

fn has_calls(graph: &Value, source_file: &str) -> bool {
    let source = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == source_file && n["label"] == "Package\\call_service")
        .unwrap();
    graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["source"] == source["id"] && e["relation"] == "calls")
}

#[test]
fn bundled_package_inherits_generated_classmap_from_containing_project() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        r#"<?php return array('Vendor\\Service'=>__DIR__.'/../library/Service.php');"#,
    );
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"library","autoload":{"psr-4":{"Package\\":"src/"}}}"#,
    );
    service(root, "vendor/library/Service.php");
    service(root, "bundled/Service.php");
    caller(root, "lib/package/src/Caller.php");
    let mut options = Options::new(root);
    options.workers = 3;
    let g = graph(&options);
    assert!(calls_file(
        &g,
        "lib/package/src/Caller.php",
        "vendor/library/Service.php"
    ));
    assert!(!calls_file(
        &g,
        "lib/package/src/Caller.php",
        "bundled/Service.php"
    ));
}

#[test]
fn inactive_package_mapping_cannot_override_containing_project_mapping() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"vendor/library/"}}}"#,
    );
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"library","autoload":{"psr-4":{"Vendor\\":"own/"}}}"#,
    );
    service(root, "vendor/library/Service.php");
    service(root, "lib/package/own/Service.php");
    caller(root, "lib/package/Caller.php");
    let g = graph(&Options::new(root));
    assert!(calls_file(
        &g,
        "lib/package/Caller.php",
        "vendor/library/Service.php"
    ));
    assert!(!calls_file(
        &g,
        "lib/package/Caller.php",
        "lib/package/own/Service.php"
    ));
}

#[test]
fn inherited_mappings_stop_at_an_explicit_project_boundary() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"vendor/library/"}}}"#,
    );
    service(root, "vendor/library/Service.php");
    put(
        root,
        "app/composer.json",
        r#"{"type":"project","autoload":{"psr-4":{"Package\\":"src/"}}}"#,
    );
    put(
        root,
        "app/lib/package/composer.json",
        r#"{"type":"library","autoload":{"psr-4":{"Own\\":"own/"}}}"#,
    );
    caller(root, "app/lib/package/Caller.php");
    let g = graph(&Options::new(root));
    assert!(!has_calls(&g, "app/lib/package/Caller.php"));
    // The inner library can still inherit the explicit project's own loader.
    put(
        root,
        "app/composer.json",
        r#"{"type":"project","autoload":{"psr-4":{"Vendor\\":"vendor/library/"}}}"#,
    );
    service(root, "app/vendor/library/Service.php");
    let g = graph(&Options::new(root));
    assert!(calls_file(
        &g,
        "app/lib/package/Caller.php",
        "app/vendor/library/Service.php"
    ));
    assert!(!calls_file(
        &g,
        "app/lib/package/Caller.php",
        "vendor/library/Service.php"
    ));
}

#[test]
fn inactive_package_authority_cannot_block_containing_project_or_unique_static_candidates() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"vendor/library/"}}}"#,
    );
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"library","config":{"classmap-authoritative":true},"autoload":{"psr-4":{"Vendor\\":"own/"}}}"#,
    );
    put(
        root,
        "lib/package/vendor/composer/autoload_classmap.php",
        "<?php return array();",
    );
    service(root, "vendor/library/Service.php");
    service(root, "lib/package/own/Service.php");
    caller(root, "lib/package/Caller.php");
    let g = graph(&Options::new(root));
    assert!(calls_file(
        &g,
        "lib/package/Caller.php",
        "vendor/library/Service.php"
    ));
    assert!(!calls_file(
        &g,
        "lib/package/Caller.php",
        "lib/package/own/Service.php"
    ));
    // An unregistered library's authority also cannot block an otherwise unique
    // static candidate once the containing project no longer selects a map.
    put(root, "composer.json", "{}");
    fs::remove_file(root.join("lib/package/own/Service.php")).unwrap();
    let g = graph(&Options::new(root));
    assert!(calls_file(
        &g,
        "lib/package/Caller.php",
        "vendor/library/Service.php"
    ));
}

#[test]
fn absent_or_excluded_explicit_project_classmap_target_cannot_rebind_ancestor_candidates() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"vendor/library/"}}}"#,
    );
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"project","config":{"classmap-authoritative":true}}"#,
    );
    put(
        root,
        "lib/package/vendor/composer/autoload_classmap.php",
        r#"<?php return array('Vendor\\Service'=>__DIR__.'/../missing/Service.php');"#,
    );
    service(root, "vendor/library/Service.php");
    service(root, "bundled/Service.php");
    caller(root, "lib/package/Caller.php");
    let g = graph(&Options::new(root));
    assert!(!has_calls(&g, "lib/package/Caller.php"));
    // The installed target is selected until extraction visibility hides it.
    service(root, "lib/package/vendor/missing/Service.php");
    let g = graph(&Options::new(root));
    assert!(calls_file(
        &g,
        "lib/package/Caller.php",
        "lib/package/vendor/missing/Service.php"
    ));
    let mut options = Options::new(root);
    options
        .exclude_file
        .push("lib/package/vendor/missing/Service.php".into());
    let g = graph(&options);
    assert!(!has_calls(&g, "lib/package/Caller.php"));
    assert!(!calls_file(
        &g,
        "lib/package/Caller.php",
        "vendor/library/Service.php"
    ));
    assert!(!calls_file(
        &g,
        "lib/package/Caller.php",
        "bundled/Service.php"
    ));
}
