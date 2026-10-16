use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn put(root: &Path, file: &str, source: &str) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

#[test]
fn statement_profiling_preserves_contextual_parent_and_duplicate_resolution() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"vendor/lib/"},"psr-0":{"Legacy_":"legacy/"}}}"#,
    );
    put(
        root,
        "vendor/lib/Base.php",
        "<?php namespace Vendor; class Base { static function run() {} }",
    );
    put(
        root,
        "copies/Base.php",
        "<?php namespace Vendor; class Base { static function run() {} }",
    );
    put(
        root,
        "vendor/lib/WrongFilename.php",
        "<?php namespace Vendor; class Base { static function run() {} }",
    );
    put(
        root,
        "legacy/Legacy/Item.php",
        "<?php class Legacy_Item { static function run() {} }",
    );
    put(
        root,
        "copies/LegacyItem.php",
        "<?php class Legacy_Item { static function run() {} }",
    );
    put(
        root,
        "legacy-caller.php",
        "<?php function legacy_call() { Legacy_Item::run(); }",
    );
    put(
        root,
        "packages/bundle/composer.json",
        r#"{"type":"library"}"#,
    );
    put(
        root,
        "packages/bundle/Child.php",
        "<?php namespace Bundle; use Vendor\\Base as ParentBase; class Child extends ParentBase { static function run() {} static function call_parent() { parent::run(); } }\nfunction call_child() { Child::run(); }",
    );
    let mut graphs = Vec::new();
    for profiling in [false, true] {
        let out = root.join(if profiling { "profile" } else { "plain" });
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphify-extractor"));
        command.arg("extract").arg(root).arg("--out").arg(&out);
        command
            .env_remove("GRE_PROFILE")
            .env_remove("GRE_PROFILE_SQL");
        if profiling {
            command.env("GRE_PROFILE", "1").env("GRE_PROFILE_SQL", "1");
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if profiling {
            let diagnostics = String::from_utf8_lossy(&result.stderr);
            assert!(diagnostics.contains("GRE_PROFILE_SQL"));
            let psr_plan: Value = diagnostics
                .lines()
                .filter_map(|line| line.strip_prefix("GRE_PROFILE_SQL "))
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find(|value| value["statement"] == "autoload.psr_selections")
                .unwrap();
            assert!(
                psr_plan["plan"].as_array().unwrap().iter().any(|step| step
                    .as_str()
                    .unwrap()
                    .contains("symbols_1 (domain=? AND name=?)")),
                "PSR physical lookup must probe exact referenced names: {psr_plan}"
            );

            assert!(diagnostics.contains("resolve.sql:CREATE TABLE class_bindings"));
            assert!(diagnostics.contains("class_contexts_lookup"));
            assert!(diagnostics.contains("php_candidates_paths (domain=? AND file>? AND file<?)"));
        }
        graphs.push(
            serde_json::from_slice::<Value>(&fs::read(out.join("graph.json")).unwrap()).unwrap(),
        );
    }
    assert_eq!(graphs[0], graphs[1]);
    let g = &graphs[1];
    let node = |file: &str, label: &str| {
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["source_file"] == file && n["label"] == label)
            .unwrap()
    };
    let parent_caller = node("packages/bundle/Child.php", "Bundle\\Child::call_parent");
    let child_caller = node("packages/bundle/Child.php", "Bundle\\call_child");
    let parent = node("vendor/lib/Base.php", "Vendor\\Base::run");
    let wrong = node("copies/Base.php", "Vendor\\Base::run");
    let child = node("packages/bundle/Child.php", "Bundle\\Child::run");
    let calls = |source: &Value, target: &Value| {
        g["links"].as_array().unwrap().iter().any(|e| {
            e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == "calls"
        })
    };
    assert!(calls(parent_caller, parent));
    assert!(!calls(parent_caller, wrong));
    assert!(!calls(parent_caller, child));
    assert!(calls(child_caller, child));
    assert!(!calls(child_caller, parent));
    assert!(!calls(
        parent_caller,
        node("vendor/lib/WrongFilename.php", "Vendor\\Base::run")
    ));
    let legacy_caller = node("legacy-caller.php", "legacy_call");
    assert!(calls(
        legacy_caller,
        node("legacy/Legacy/Item.php", "Legacy_Item::run")
    ));
    assert!(!calls(
        legacy_caller,
        node("copies/LegacyItem.php", "Legacy_Item::run")
    ));
}
