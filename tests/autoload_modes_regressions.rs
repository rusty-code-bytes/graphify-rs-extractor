use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, file: &str, source: impl AsRef<[u8]>) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}
fn graph(options: &Options) -> Value {
    extract(options).unwrap();
    serde_json::from_slice(&fs::read(options.root.join("graphify-out/graph.json")).unwrap())
        .unwrap()
}
fn node<'a>(g: &'a Value, file: &str, label: &str) -> &'a Value {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == file && n["label"] == label)
        .unwrap_or_else(|| panic!("missing {file}::{label}"))
}
fn edge(g: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    g["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == relation
    })
}
fn calls(g: &Value, source: &Value) -> bool {
    g["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["source"] == source["id"] && e["relation"] == "calls")
}
fn generated(root: &Path, name: &str, entries: &str) {
    put(
        root,
        &format!("vendor/composer/autoload_{name}.php"),
        format!(
            "<?php $vendorDir=dirname(__DIR__); $baseDir=dirname($vendorDir); return array({entries});"
        ),
    );
}

#[test]
fn generated_psr0_selects_duplicate_classes_and_inherited_methods_with_excluded_source_fallback() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(
        root,
        "namespaces",
        r#"'HTMLPurifier'=>array($vendorDir.'/purifier/library'),"#,
    );
    for base in ["vendor/purifier/library", "bundled"] {
        put(
            root,
            &format!("{base}/HTMLPurifier/AttrDef.php"),
            "<?php abstract class HTMLPurifier_AttrDef { function parseCDATA($value) {} }",
        );
        put(
            root,
            &format!("{base}/HTMLPurifier/URIParser.php"),
            "<?php class HTMLPurifier_URIParser {}",
        );
    }
    put(
        root,
        "app.php",
        "<?php class App extends HTMLPurifier_AttrDef { function run() { $this->parseCDATA(''); new HTMLPurifier_URIParser; } }",
    );
    for excluded in [false, true] {
        let mut options = Options::new(root);
        options.workers = 2;
        if excluded {
            options.exclude.push("vendor/**".into());
        }
        let g = graph(&options);
        let base = if excluded {
            "bundled"
        } else {
            "vendor/purifier/library"
        };
        assert!(edge(
            &g,
            node(&g, "app.php", "App"),
            node(
                &g,
                &format!("{base}/HTMLPurifier/AttrDef.php"),
                "HTMLPurifier_AttrDef"
            ),
            "extends"
        ));
        assert!(edge(
            &g,
            node(&g, "app.php", "App::run"),
            node(
                &g,
                &format!("{base}/HTMLPurifier/AttrDef.php"),
                "HTMLPurifier_AttrDef::parseCDATA"
            ),
            "calls"
        ));
        assert!(edge(
            &g,
            node(&g, "app.php", "App::run"),
            node(
                &g,
                &format!("{base}/HTMLPurifier/URIParser.php"),
                "HTMLPurifier_URIParser"
            ),
            "instantiates"
        ));
        if !excluded {
            assert!(!edge(
                &g,
                node(&g, "app.php", "App"),
                node(
                    &g,
                    "bundled/HTMLPurifier/AttrDef.php",
                    "HTMLPurifier_AttrDef"
                ),
                "extends"
            ));
        }
    }
}

#[test]
fn source_manifest_psr0_and_files_inherit_within_project_but_stop_at_project_boundary() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-0":{"Legacy":"legacy/"},"files":["functions.php"]}}"#,
    );
    put(
        root,
        "legacy/Legacy/Service.php",
        "<?php class Legacy_Service { static function work() {} }",
    );
    put(
        root,
        "copy.php",
        "<?php class Legacy_Service { static function work() {} } function assist() {}",
    );
    put(root, "functions.php", "<?php function assist() {}");
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"library","autoload":{"psr-0":{"Package":"src/"}}}"#,
    );
    put(
        root,
        "lib/package/caller.php",
        "<?php function run() { Legacy_Service::work(); assist(); }",
    );
    put(root, "project/composer.json", r#"{"type":"project"}"#);
    put(
        root,
        "project/caller.php",
        "<?php function run() { Legacy_Service::work(); assist(); }",
    );
    let g = graph(&Options::new(root));
    let run = node(&g, "lib/package/caller.php", "run");
    assert!(edge(
        &g,
        run,
        node(&g, "legacy/Legacy/Service.php", "Legacy_Service::work"),
        "calls"
    ));
    assert!(edge(&g, run, node(&g, "functions.php", "assist"), "calls"));
    assert!(!calls(&g, node(&g, "project/caller.php", "run")));
}

#[test]
fn generated_files_select_exact_imported_and_global_functions_preserving_local_declarations() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(
        root,
        "files",
        r#"'vendor-functions'=>$vendorDir.'/library/functions.php',"#,
    );
    let source =
        "<?php namespace Vendor { function assist() {} } namespace { function helper() {} }";
    put(root, "vendor/library/functions.php", source);
    put(root, "bundled/functions.php", source);
    put(
        root,
        "caller.php",
        "<?php namespace App; use function Vendor\\assist as help; function run() { help(); helper(); }",
    );
    put(
        root,
        "local.php",
        "<?php namespace Vendor; function assist() {} function local_call() { assist(); }",
    );
    for excluded in [false, true] {
        let mut options = Options::new(root);
        options.workers = 3;
        if excluded {
            options.exclude.push("vendor/**".into());
            options.exclude_file.push("local.php".into());
        }
        let g = graph(&options);
        let file = if excluded {
            "bundled/functions.php"
        } else {
            "vendor/library/functions.php"
        };
        let run = node(&g, "caller.php", "App\\run");
        assert!(edge(&g, run, node(&g, file, "Vendor\\assist"), "calls"));
        assert!(edge(&g, run, node(&g, file, "helper"), "calls"));
        if !excluded {
            assert!(!edge(
                &g,
                run,
                node(&g, "bundled/functions.php", "Vendor\\assist"),
                "calls"
            ));
            assert!(edge(
                &g,
                node(&g, "local.php", "Vendor\\local_call"),
                node(&g, "local.php", "Vendor\\assist"),
                "calls"
            ));
        }
    }
}

#[test]
fn competing_listed_functions_are_ambiguous_while_repeated_same_file_is_not() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"files":["first.php","second.php"]}}"#,
    );
    for file in ["first.php", "second.php"] {
        put(root, file, "<?php namespace Lib; function assist() {}");
    }
    put(
        root,
        "caller.php",
        "<?php function run() { \\Lib\\assist(); }",
    );
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "run")));
    put(
        root,
        "composer.json",
        r#"{"autoload":{"files":["first.php","first.php"]}}"#,
    );
    let g = graph(&Options::new(root));
    assert!(edge(
        &g,
        node(&g, "caller.php", "run"),
        node(&g, "first.php", "Lib\\assist"),
        "calls"
    ));
}

#[test]
fn autoload_files_never_use_leaf_names_or_bypass_an_ambiguous_namespaced_function() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"files":["global.php"]}}"#,
    );
    put(root, "global.php", "<?php function helper() {}");
    put(root, "a.php", "<?php namespace App; function helper() {}");
    put(root, "b.php", "<?php namespace App; function helper() {}");
    put(
        root,
        "caller.php",
        "<?php namespace App; use function Missing\\helper as imported; function run() { helper(); } function wrong_import() { imported(); }",
    );
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "App\\run")));
    assert!(!calls(&g, node(&g, "caller.php", "App\\wrong_import")));
}

#[test]
fn psr0_missing_exact_fqn_or_mismatching_filename_never_selects_a_same_leaf_class() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-0":{"Expected":"vendor/legacy/"}}}"#,
    );
    put(
        root,
        "vendor/legacy/Expected/Service.php",
        "<?php class Unrelated_Service { static function work() {} }",
    );
    put(
        root,
        "first.php",
        "<?php class Expected_Service { static function work() {} }",
    );
    put(
        root,
        "second.php",
        "<?php class Expected_Service { static function work() {} }",
    );
    put(
        root,
        "caller.php",
        "<?php function run() { Expected_Service::work(); } function absent() { Missing_Service::work(); }",
    );
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "run")));
    assert!(!calls(&g, node(&g, "caller.php", "absent")));
}

#[test]
fn unrelated_sibling_loader_is_not_attached_to_root_references() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    put(
        root,
        "external/composer.json",
        r#"{"config":{"vendor-dir":"."},"autoload":{"psr-0":{"Legacy":"classes/"},"files":["functions.php"]}}"#,
    );
    put(
        root,
        "external/classes/Legacy/Service.php",
        "<?php class Legacy_Service { static function work() {} }",
    );
    put(root, "external/functions.php", "<?php function assist() {}");
    put(
        root,
        "stubs.php",
        "<?php class Legacy_Service { static function work() {} } function assist() {}",
    );
    put(
        root,
        "caller.php",
        "<?php function run() { Legacy_Service::work(); assist(); }",
    );
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "run")));
}

#[test]
fn psr0_does_not_alias_raw_identifier_bytes_to_valid_unicode_filename() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-0":{"Legacy":"legacy/"}}}"#,
    );
    let mut class = b"<?php class Legacy_Name".to_vec();
    class.push(0xa9);
    class.extend_from_slice(b" { static function work() {} }");
    let mut caller = b"<?php function run() { Legacy_Name".to_vec();
    caller.push(0xa9);
    caller.extend_from_slice(b"::work(); }");
    put(root, "caller.php", caller);
    put(root, "other.php", &class);
    put(root, "legacy/Legacy/Name\u{f100}\u{e0a9}.php", class);
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "run")));
}

#[test]
fn autoload_file_early_return_abstains_without_overriding_other_definitions_or_local_binding() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"files":["registered.php"]}}"#,
    );
    put(
        root,
        "registered.php",
        r#"<?php namespace Lib;
if ($runtimeVersion) { return require __DIR__ . '/delegate.php'; }
if (!function_exists('Lib\\helper')) { function helper() { return 1; } }
function local_call() { helper(); }
"#,
    );
    put(
        root,
        "delegate.php",
        "<?php namespace Lib; function helper() { return 2; }",
    );
    put(
        root,
        "caller.php",
        "<?php function run() { \\Lib\\helper(); }",
    );
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "run")));
    assert!(edge(
        &g,
        node(&g, "registered.php", "Lib\\local_call"),
        node(&g, "registered.php", "Lib\\helper"),
        "calls"
    ));
    // A single remaining declaration is still not evidence that a wrapper's
    // conditional branch defines it for an external caller.
    fs::remove_file(root.join("delegate.php")).unwrap();
    let g = graph(&Options::new(root));
    assert!(!calls(&g, node(&g, "caller.php", "run")));
}

#[test]
fn function_exists_guard_and_function_body_returns_remain_eligible_for_registered_file_selection() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"files":["registered.php"]}}"#,
    );
    put(
        root,
        "registered.php",
        r#"<?php namespace Lib;
if (!function_exists('Lib\\helper')) { function helper() { if (true) { return 1; } return 2; } }
class Other { function method() { return 3; } }
$closure = function () { return 4; };
"#,
    );
    put(
        root,
        "other.php",
        "<?php namespace Lib; function helper() {}",
    );
    put(
        root,
        "caller.php",
        "<?php function run() { \\Lib\\helper(); }",
    );
    let g = graph(&Options::new(root));
    assert!(edge(
        &g,
        node(&g, "caller.php", "run"),
        node(&g, "registered.php", "Lib\\helper"),
        "calls"
    ));
    assert!(!edge(
        &g,
        node(&g, "caller.php", "run"),
        node(&g, "other.php", "Lib\\helper"),
        "calls"
    ));
}

#[test]
fn generated_registered_functions_precede_bare_package_manifest_but_not_another_generated_loader() {
    for (child_generated, excluded, project) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (false, false, true),
    ] {
        let d = TempDir::new().unwrap();
        let root = d.path();
        put(root, "composer.json", "{}");
        generated(
            root,
            "files",
            r#"'registered'=>$vendorDir.'/library/functions.php',"#,
        );
        put(
            root,
            "vendor/library/functions.php",
            "<?php namespace Aws; function describe_type($value) { return 'vendor'; }",
        );
        put(
            root,
            "lib/package/composer.json",
            if project {
                r#"{"type":"project","autoload":{"files":["src/functions.php"]}}"#
            } else {
                r#"{"type":"library","autoload":{"files":["src/functions.php"]}}"#
            },
        );
        put(
            root,
            "lib/package/src/functions.php",
            "<?php namespace Aws; function describe_type($value) { return 'bundle'; } function local_call() { describe_type(null); }",
        );
        put(
            root,
            "lib/package/src/Caller.php",
            "<?php namespace Aws; function caller() { describe_type(null); }",
        );
        if child_generated {
            put(
                root,
                "lib/package/vendor/composer/autoload_files.php",
                r#"<?php return ['child'=>__DIR__.'/../../src/functions.php'];"#,
            );
        }
        let mut options = Options::new(root);
        options.workers = 2;
        if excluded {
            options.exclude.push("vendor/**".into());
        }
        let g = graph(&options);
        let caller = node(&g, "lib/package/src/Caller.php", "Aws\\caller");
        if child_generated {
            assert!(
                !calls(&g, caller),
                "two registered function files must remain ambiguous"
            );
        } else {
            let target = if excluded || project {
                "lib/package/src/functions.php"
            } else {
                "vendor/library/functions.php"
            };
            assert!(
                edge(&g, caller, node(&g, target, "Aws\\describe_type"), "calls"),
                "excluded={excluded}, project={project}"
            );
            if !excluded {
                assert_eq!(
                    edge(
                        &g,
                        caller,
                        node(&g, "vendor/library/functions.php", "Aws\\describe_type"),
                        "calls"
                    ),
                    !project
                );
            }
        }
        assert!(edge(
            &g,
            node(&g, "lib/package/src/functions.php", "Aws\\local_call"),
            node(&g, "lib/package/src/functions.php", "Aws\\describe_type"),
            "calls"
        ));
    }
}

#[test]
fn unsafe_generated_function_evidence_blocks_source_fallback_but_invalid_metadata_does_not_claim_provenance()
 {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(
        root,
        "files",
        r#"'registered'=>$vendorDir.'/library/functions.php',"#,
    );
    put(
        root,
        "vendor/library/functions.php",
        "<?php namespace Aws; if ($condition) { return; } function describe_type($value) {}",
    );
    put(
        root,
        "lib/package/composer.json",
        r#"{"type":"library","autoload":{"files":["src/functions.php"]}}"#,
    );
    put(
        root,
        "lib/package/src/functions.php",
        "<?php namespace Aws; function describe_type($value) {}",
    );
    put(
        root,
        "lib/package/src/Caller.php",
        "<?php namespace Aws; function caller() { describe_type(null); }",
    );
    let g = graph(&Options::new(root));
    assert!(!calls(
        &g,
        node(&g, "lib/package/src/Caller.php", "Aws\\caller")
    ));
    // Whole-map rejection must discard its stronger provenance too.
    generated(
        root,
        "files",
        r#"'registered'=>$vendorDir.'/library/functions.php','dynamic'=>unknown(),"#,
    );
    let g = graph(&Options::new(root));
    assert!(edge(
        &g,
        node(&g, "lib/package/src/Caller.php", "Aws\\caller"),
        node(&g, "lib/package/src/functions.php", "Aws\\describe_type"),
        "calls"
    ));
}
