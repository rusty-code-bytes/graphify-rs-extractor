use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, source).unwrap();
}
fn graph(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("graphify-out/graph.json")).unwrap()).unwrap()
}
fn node<'a>(g: &'a Value, kind: &str, label: &str) -> &'a Value {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["type"] == kind && n["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label}: {g}"))
}
fn calls(g: &Value, source: &Value, target: &Value) -> bool {
    g["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == "calls"
    })
}

#[test]
fn uninstalled_bundled_package_manifest_does_not_hide_unique_classes_or_functions() {
    for workers in [1, 8] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(root, "composer.json", "{}");
        put(
            root,
            "caller.php",
            "<?php function caller() { Vendor\\Thing::run(); Vendor\\helper(); }",
        );
        put(
            root,
            "public/lib/pkg/composer.json",
            r#"{"name":"vendor/thing","type":"library","autoload":{"psr-4":{"Vendor\\":"src/"}}}"#,
        );
        put(
            root,
            "public/lib/pkg/src/Thing.php",
            "<?php namespace Vendor; class Thing { static function run() {} } function helper() {}",
        );
        put(
            root,
            "decoy.php",
            "<?php namespace Other; class Thing { static function run() {} } function helper() {}",
        );
        let mut options = Options::new(root);
        options.workers = workers;
        extract(&options).unwrap();
        let g = graph(root);
        let caller = node(&g, "function", "caller");
        assert!(calls(&g, caller, node(&g, "method", "Vendor\\Thing::run")));
        assert!(calls(&g, caller, node(&g, "function", "Vendor\\helper")));
        assert!(!calls(&g, caller, node(&g, "method", "Other\\Thing::run")));
        assert!(!calls(&g, caller, node(&g, "function", "Other\\helper")));
    }
}

#[test]
fn explicit_classmap_reaches_exact_file_inside_nested_loader_context() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(root, "composer.json", "{}");
    put(
        root,
        "caller.php",
        "<?php function caller() { Vendor\\Thing::run(); }",
    );
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        "<?php return ['Vendor\\\\Thing' => __DIR__ . '/../../packages/nested/Thing.php'];",
    );
    put(
        root,
        "packages/nested/composer.json",
        r#"{"name":"example/nested","type":"project"}"#,
    );
    put(
        root,
        "packages/nested/vendor/composer/autoload_classmap.php",
        "<?php return ['Vendor\\\\Thing' => __DIR__ . '/../../Thing.php'];",
    );
    put(
        root,
        "packages/nested/Thing.php",
        "<?php namespace Vendor; class Thing { static function run() {} }",
    );
    put(
        root,
        "decoy.php",
        "<?php namespace Decoy; class Thing { static function run() {} }",
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let caller = node(&g, "function", "caller");
    assert!(calls(&g, caller, node(&g, "method", "Vendor\\Thing::run")));
    assert!(!calls(&g, caller, node(&g, "method", "Decoy\\Thing::run")));
}

#[test]
fn absent_classmap_target_never_binds_an_unrelated_same_fqn_file() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(root, "composer.json", "{}");
    put(
        root,
        "caller.php",
        "<?php function caller() { Target::run(); }",
    );
    put(
        root,
        "Other.php",
        "<?php class Target { static function run() {} }",
    );
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        "<?php return ['Target' => __DIR__ . '/Missing.php'];",
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    assert!(!calls(
        &g,
        node(&g, "function", "caller"),
        node(&g, "method", "Target::run")
    ));
}

#[test]
fn generated_loader_false_or_unreachable_true_cannot_disable_psr4() {
    let cases = [
        (
            "{}",
            "ComposerAutoloaderInit123",
            "$loader->setClassMapAuthoritative(true); $loader->setClassMapAuthoritative(false); return $loader;",
        ),
        (
            "{}",
            "ComposerAutoloaderInit123",
            "return $loader; $loader->setClassMapAuthoritative(true);",
        ),
        (
            "{}",
            "UnrelatedUtility",
            "$loader->setClassMapAuthoritative(true); return $loader;",
        ),
        (
            r#"{"config":{"classmap-authoritative":true}}"#,
            "ComposerAutoloaderInit123",
            "$loader->setClassMapAuthoritative(false); return $loader;",
        ),
    ];
    for (manifest, class, body) in cases {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(root, "composer.json", manifest);
        put(
            root,
            "caller.php",
            "<?php function caller() { Target::run(); }",
        );
        put(
            root,
            "Target.php",
            "<?php class Target { static function run() {} }",
        );
        put(
            root,
            "Other.php",
            "<?php namespace Other; class Target { static function run() {} }",
        );
        put(
            root,
            "vendor/composer/autoload_classmap.php",
            "<?php return [];",
        );
        put(
            root,
            "vendor/composer/autoload_psr4.php",
            "<?php return ['' => [__DIR__ . '/../..']];",
        );
        put(
            root,
            "vendor/composer/autoload_real.php",
            &format!("<?php class {class} {{ static function getLoader() {{ {body} }} }}"),
        );
        extract(&Options::new(root)).unwrap();
        let g = graph(root);
        let caller = node(&g, "function", "caller");
        assert!(
            calls(&g, caller, node(&g, "method", "Target::run")),
            "manifest={manifest} class={class} body={body}"
        );
        assert!(!calls(&g, caller, node(&g, "method", "Other\\Target::run")));
    }
}

#[test]
fn classmap_case_collisions_abstain_without_aborting_graph_publication() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(root, "composer.json", "{}");
    put(
        root,
        "caller.php",
        "<?php function caller() { Target::run(); }",
    );
    put(
        root,
        "A.php",
        "<?php class Target { static function run() {} }",
    );
    put(
        root,
        "B.php",
        "<?php class target { static function run() {} }",
    );
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        "<?php return ['Target' => __DIR__ . '/../../A.php', 'target' => __DIR__ . '/../../B.php'];",
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let caller = node(&g, "function", "caller");
    assert!(!calls(&g, caller, node(&g, "method", "Target::run")));
    assert!(!calls(&g, caller, node(&g, "method", "target::run")));
}

#[test]
fn psr4_invalid_identifier_bytes_do_not_alias_valid_unicode_filenames() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Byte\\":"src/"}}}"#,
    );
    let deceptive = "src/\u{f100}\u{e0a9}.php".to_string();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    let mut definition = b"<?php namespace Byte; class ".to_vec();
    definition.push(0xa9);
    definition.extend(b" { static function run() {} }");
    fs::write(root.join(&deceptive), &definition).unwrap();
    fs::write(root.join("other/ValueWrapper.php"), &definition).unwrap();
    let mut caller = b"<?php function caller() { Byte\\".to_vec();
    caller.push(0xa9);
    caller.extend(b"::run(); }");
    fs::write(root.join("caller.php"), caller).unwrap();
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let caller = node(&g, "function", "caller");
    assert_eq!(
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] == "method")
            .count(),
        2
    );
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == caller["id"] && e["relation"] == "calls")
    );
}
