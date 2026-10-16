use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, file: &str, source: &str) {
    let target = root.join(file);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, source).unwrap();
}
fn graph(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("graphify-out/graph.json")).unwrap()).unwrap()
}
fn entity<'a>(g: &'a Value, file: &str, label: &str) -> &'a Value {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == file && n["label"] == label)
        .unwrap_or_else(|| panic!("missing {file}: {label}"))
}
fn calls(g: &Value, source: &Value, target: &Value) -> bool {
    g["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == "calls"
    })
}
fn service(root: &Path, file: &str) {
    put(
        root,
        file,
        "<?php namespace Legacy_Ns\\Package; class Some_Class { static function run() {} }",
    );
}

#[test]
fn psr0_keeps_full_namespace_and_namespace_underscores_with_classmap_and_psr4_precedence() {
    for mode in ["psr0", "psr4", "classmap"] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(root, "composer.json", "{}");
        put(
            root,
            "caller.php",
            "<?php function caller() { Legacy_Ns\\Package\\Some_Class::run(); }",
        );
        // PSR-0 includes the namespace prefix in the relative path, preserves
        // underscores in namespace segments, and splits only class underscores.
        service(root, "legacy/Legacy_Ns/Package/Some/Class.php");
        service(root, "legacy/Some/Class.php");
        service(root, "legacy/Legacy/Ns/Package/Some/Class.php");
        service(root, "fallback/Legacy_Ns/Package/Some/Class.php");
        service(root, "modern/Some_Class.php");
        service(root, "mapped/Chosen.php");
        put(
            root,
            "vendor/composer/autoload_namespaces.php",
            "<?php return ['' => [__DIR__ . '/../../fallback'], 'Legacy_Ns\\\\Package\\\\' => [__DIR__ . '/../../legacy']];",
        );
        if matches!(mode, "psr4" | "classmap") {
            put(
                root,
                "vendor/composer/autoload_psr4.php",
                "<?php return ['Legacy_Ns\\\\Package\\\\' => [__DIR__ . '/../../modern']];",
            );
        }
        if mode == "classmap" {
            put(
                root,
                "vendor/composer/autoload_classmap.php",
                "<?php return ['Legacy_Ns\\\\Package\\\\Some_Class' => __DIR__ . '/../../mapped/Chosen.php'];",
            );
        }
        extract(&Options::new(root)).unwrap();
        let g = graph(root);
        let source = entity(&g, "caller.php", "caller");
        let expected = match mode {
            "psr0" => "legacy/Legacy_Ns/Package/Some/Class.php",
            "psr4" => "modern/Some_Class.php",
            "classmap" => "mapped/Chosen.php",
            _ => unreachable!(),
        };
        for file in [
            "legacy/Legacy_Ns/Package/Some/Class.php",
            "legacy/Some/Class.php",
            "legacy/Legacy/Ns/Package/Some/Class.php",
            "modern/Some_Class.php",
            "fallback/Legacy_Ns/Package/Some/Class.php",
            "mapped/Chosen.php",
        ] {
            assert_eq!(
                calls(
                    &g,
                    source,
                    entity(&g, file, "Legacy_Ns\\Package\\Some_Class::run")
                ),
                file == expected,
                "mode={mode}, file={file}"
            );
        }
    }
}

#[test]
fn inactive_library_autoload_files_cannot_override_root_functions_or_enable_global_fallback() {
    for workers in [1, 4] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(root, "composer.json", "{}");
        put(
            root,
            "vendor/composer/autoload_files.php",
            "<?php return ['root-id' => __DIR__ . '/../../root_functions.php'];",
        );
        put(
            root,
            "root_functions.php",
            "<?php namespace Lib; function shared() {} function only_root() {}",
        );
        put(root, "global.php", "<?php function shared() {}");
        put(root, "lib/package/composer.json", r#"{"type":"library"}"#);
        put(
            root,
            "lib/package/vendor/composer/autoload_files.php",
            "<?php return ['child-id' => __DIR__ . '/../../child_functions.php'];",
        );
        put(
            root,
            "lib/package/child_functions.php",
            "<?php namespace Lib; function shared() {}",
        );
        put(
            root,
            "lib/package/caller.php",
            "<?php namespace Lib; function namespaced_caller() { shared(); only_root(); } namespace Consumer; use function Lib\\shared as invoke; function aliased_caller() { invoke(); }",
        );
        let mut options = Options::new(root);
        options.workers = workers;
        extract(&options).unwrap();
        let g = graph(root);
        let namespaced = entity(&g, "lib/package/caller.php", "Lib\\namespaced_caller");
        let aliased = entity(&g, "lib/package/caller.php", "Consumer\\aliased_caller");
        assert!(calls(
            &g,
            namespaced,
            entity(&g, "root_functions.php", "Lib\\only_root")
        ));
        for source in [namespaced, aliased] {
            assert!(calls(
                &g,
                source,
                entity(&g, "root_functions.php", "Lib\\shared")
            ));
            for (file, label) in [
                ("lib/package/child_functions.php", "Lib\\shared"),
                ("global.php", "shared"),
            ] {
                assert!(
                    !calls(&g, source, entity(&g, file, label)),
                    "workers={workers}, target={file}"
                );
            }
        }
    }
}

#[test]
fn dynamic_generated_files_metadata_never_publishes_partial_choices_or_executes_php() {
    for list in [
        "return ['good' => __DIR__ . '/../../first.php', 'dynamic' => choose()];",
        "file_put_contents(__DIR__ . '/executed', 'bad'); return ['good' => __DIR__ . '/../../first.php'];",
        "return ['good' => __DIR__ . '/../../first.php', 'outside' => '/outside/root.php'];",
    ] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(root, "composer.json", "{}");
        put(
            root,
            "first.php",
            "<?php namespace Lib; function helper() {}",
        );
        put(
            root,
            "second.php",
            "<?php namespace Lib; function helper() {}",
        );
        put(
            root,
            "caller.php",
            "<?php function caller() { Lib\\helper(); }",
        );
        put(
            root,
            "vendor/composer/autoload_files.php",
            &format!("<?php {list}"),
        );
        extract(&Options::new(root)).unwrap();
        let g = graph(root);
        let caller = entity(&g, "caller.php", "caller");
        assert!(
            !calls(&g, caller, entity(&g, "first.php", "Lib\\helper")),
            "partial choice for {list}"
        );
        assert!(!calls(&g, caller, entity(&g, "second.php", "Lib\\helper")));
        assert!(!root.join("vendor/composer/executed").exists());
    }
}
