use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: impl AsRef<[u8]>) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn run(options: &Options) -> Value {
    extract(options).unwrap();
    serde_json::from_slice(&fs::read(options.root.join("graphify-out/graph.json")).unwrap())
        .unwrap()
}

fn node<'a>(graph: &'a Value, file: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["source_file"] == file && node["label"] == label)
        .unwrap_or_else(|| panic!("missing {file}::{label}"))
}

fn linked(graph: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    graph["links"].as_array().unwrap().iter().any(|edge| {
        edge["source"] == source["id"]
            && edge["target"] == target["id"]
            && edge["relation"] == relation
    })
}

fn has_calls(graph: &Value, source: &Value) -> bool {
    graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|edge| edge["source"] == source["id"] && edge["relation"] == "calls")
}

fn generated(root: &Path, context: &str, name: &str, entries: &str) {
    put(
        root,
        &format!("{context}vendor/composer/autoload_{name}.php"),
        format!(
            "<?php\n$vendorDir = dirname(__DIR__);\n$baseDir = dirname($vendorDir);\nreturn array(\n{entries}\n);\n"
        ),
    );
}

#[test]
fn installed_composer_psr4_restores_duplicate_edges_and_excluded_vendor_uses_visible_fallback() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(
        root,
        "",
        "psr4",
        r#"'GuzzleHttp\\' => array($vendorDir . '/guzzlehttp/guzzle/src'),"#,
    );
    for base in [
        "vendor/guzzlehttp/guzzle/src",
        "public/lib/guzzlehttp/guzzle/src",
    ] {
        put(
            root,
            &format!("{base}/Client.php"),
            "<?php namespace GuzzleHttp; class Client { function __construct() {} }",
        );
        put(
            root,
            &format!("{base}/HandlerStack.php"),
            "<?php namespace GuzzleHttp; class HandlerStack { static function create() {} }",
        );
    }
    put(
        root,
        "public/lib/classes/http_client.php",
        r#"<?php
namespace core;
use GuzzleHttp\Client;
use GuzzleHttp\HandlerStack;
class http_client extends Client {
 function __construct() { parent::__construct(); }
 function get_handlers() { HandlerStack::create(); }
}
function dispatch() { HandlerStack::create(); }
dispatch();
"#,
    );
    for excluded in [false, true] {
        let mut options = Options::new(root);
        options.workers = 2;
        if excluded {
            options.exclude.push("vendor/**".into());
        }
        let graph = run(&options);
        let base = if excluded {
            "public/lib/guzzlehttp/guzzle/src"
        } else {
            "vendor/guzzlehttp/guzzle/src"
        };
        let source = "public/lib/classes/http_client.php";
        assert!(linked(
            &graph,
            node(&graph, source, "core\\http_client"),
            node(&graph, &format!("{base}/Client.php"), "GuzzleHttp\\Client"),
            "extends"
        ));
        assert!(linked(
            &graph,
            node(&graph, source, "core\\http_client::__construct"),
            node(
                &graph,
                &format!("{base}/Client.php"),
                "GuzzleHttp\\Client::__construct"
            ),
            "calls"
        ));
        assert!(linked(
            &graph,
            node(&graph, source, "core\\http_client::get_handlers"),
            node(
                &graph,
                &format!("{base}/HandlerStack.php"),
                "GuzzleHttp\\HandlerStack::create"
            ),
            "calls"
        ));
        assert!(linked(
            &graph,
            node(&graph, source, source),
            node(&graph, source, "core\\dispatch"),
            "calls"
        ));
        if !excluded {
            assert!(!linked(
                &graph,
                node(&graph, source, "core\\http_client::get_handlers"),
                node(
                    &graph,
                    "public/lib/guzzlehttp/guzzle/src/HandlerStack.php",
                    "GuzzleHttp\\HandlerStack::create"
                ),
                "calls"
            ));
        }
    }
}

#[test]
fn duplicate_fqns_without_mapping_remain_ambiguous_and_vendor_is_not_special() {
    let d = TempDir::new().unwrap();
    put(d.path(), "composer.json", "{}");
    for path in ["vendor/pkg/A.php", "bundled/A.php"] {
        put(
            d.path(),
            path,
            "<?php namespace Lib; class A { static function work() {} }",
        );
    }
    put(
        d.path(),
        "caller.php",
        "<?php function run() { \\Lib\\A::work(); new \\Lib\\A; }",
    );
    let graph = run(&Options::new(d.path()));
    let caller = node(&graph, "caller.php", "run");
    assert!(!has_calls(&graph, caller));
    assert!(
        !graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == caller["id"] && edge["relation"] == "instantiates")
    );
}

#[test]
fn source_psr4_uses_longest_prefix_then_directory_order_and_indexed_fallback() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Demo\\":["first/","second/"],"Demo\\Special\\":"special/"}}}"#,
    );
    for path in ["first/A.php", "second/A.php", "elsewhere/A.php"] {
        put(
            root,
            path,
            "<?php namespace Demo; class A { static function work() {} }",
        );
    }
    for path in ["first/Special/B.php", "special/B.php"] {
        put(
            root,
            path,
            "<?php namespace Demo\\Special; class B { static function work() {} }",
        );
    }
    put(
        root,
        "caller.php",
        "<?php function run() { \\Demo\\A::work(); \\Demo\\Special\\B::work(); }",
    );
    let graph = run(&Options::new(root));
    let caller = node(&graph, "caller.php", "run");
    assert!(linked(
        &graph,
        caller,
        node(&graph, "first/A.php", "Demo\\A::work"),
        "calls"
    ));
    assert!(linked(
        &graph,
        caller,
        node(&graph, "special/B.php", "Demo\\Special\\B::work"),
        "calls"
    ));
    assert!(!linked(
        &graph,
        caller,
        node(&graph, "first/Special/B.php", "Demo\\Special\\B::work"),
        "calls"
    ));
    let mut options = Options::new(root);
    options.exclude_file.push("first/A.php".into());
    let graph = run(&options);
    assert!(linked(
        &graph,
        node(&graph, "caller.php", "run"),
        node(&graph, "second/A.php", "Demo\\A::work"),
        "calls"
    ));
}

#[test]
fn generated_classmap_precedes_psr4_and_local_declarations_precede_both() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(
        root,
        "",
        "classmap",
        r#"'Demo\\A' => $baseDir . '/mapped.php',"#,
    );
    generated(root, "", "psr4", r#"'Demo\\' => array($baseDir . '/src'),"#);
    for path in ["mapped.php", "src/A.php"] {
        put(
            root,
            path,
            "<?php namespace Demo; class A { static function work() {} function local() { self::work(); } }",
        );
    }
    put(
        root,
        "caller.php",
        "<?php function run() { \\Demo\\A::work(); }",
    );
    let graph = run(&Options::new(root));
    assert!(linked(
        &graph,
        node(&graph, "caller.php", "run"),
        node(&graph, "mapped.php", "Demo\\A::work"),
        "calls"
    ));
    for path in ["mapped.php", "src/A.php"] {
        assert!(linked(
            &graph,
            node(&graph, path, "Demo\\A::local"),
            node(&graph, path, "Demo\\A::work"),
            "calls"
        ));
    }
}

#[test]
fn authoritative_classmap_blocks_unlisted_nonlocal_classes_but_preserves_local_and_excluded_targets()
 {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"config":{"classmap-authoritative":true},"autoload":{"psr-4":{"Demo\\":"src/"}}}"#,
    );
    generated(
        root,
        "",
        "classmap",
        r#"'Demo\\Listed' => $vendorDir . '/pkg/Listed.php',"#,
    );
    for path in ["vendor/pkg/Listed.php", "bundled/Listed.php"] {
        put(
            root,
            path,
            "<?php namespace Demo; class Listed { static function work() {} }",
        );
    }
    put(
        root,
        "src/Unlisted.php",
        "<?php namespace Demo; class Unlisted { static function work() {} function local() { self::work(); } }",
    );
    put(
        root,
        "caller.php",
        "<?php function listed() { \\Demo\\Listed::work(); } function unlisted() { \\Demo\\Unlisted::work(); }",
    );
    let mut options = Options::new(root);
    options.exclude_file.push("vendor/pkg/Listed.php".into());
    let graph = run(&options);
    assert!(linked(
        &graph,
        node(&graph, "caller.php", "listed"),
        node(&graph, "bundled/Listed.php", "Demo\\Listed::work"),
        "calls"
    ));
    assert!(!has_calls(&graph, node(&graph, "caller.php", "unlisted")));
    assert!(linked(
        &graph,
        node(&graph, "src/Unlisted.php", "Demo\\Unlisted::local"),
        node(&graph, "src/Unlisted.php", "Demo\\Unlisted::work"),
        "calls"
    ));
}

#[test]
fn generated_authority_requires_a_literal_unconditional_call_and_complete_map() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(root, "", "classmap", "");
    put(
        root,
        "Only.php",
        "<?php class Only { static function work() {} }",
    );
    put(root, "caller.php", "<?php function run() { Only::work(); }");
    for (real, blocked) in [
        (
            "<?php class ComposerAutoloaderInit { static function getLoader() { $loader->setClassMapAuthoritative(true); } }",
            true,
        ),
        (
            "<?php class ComposerAutoloaderInit { static function getLoader() { if ($condition) { $loader->setClassMapAuthoritative(true); } } }",
            false,
        ),
        (
            "<?php class ComposerAutoloaderInit { static function getLoader() { $loader->setClassMapAuthoritative($dynamic); } }",
            false,
        ),
    ] {
        put(root, "vendor/composer/autoload_real.php", real);
        let graph = run(&Options::new(root));
        assert_eq!(
            !has_calls(&graph, node(&graph, "caller.php", "run")),
            blocked
        );
    }
    // An unsupported classmap invalidates authority even if the source config
    // requests it; extraction never evaluates its expressions.
    put(
        root,
        "composer.json",
        r#"{"config":{"classmap-authoritative":true}}"#,
    );
    generated(root, "", "classmap", r#"'Only' => getenv('ANYTHING'),"#);
    let graph = run(&Options::new(root));
    assert!(has_calls(&graph, node(&graph, "caller.php", "run")));
}

#[test]
fn dynamic_or_partial_maps_never_choose_an_arbitrary_duplicate_or_execute_php() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    for path in ["first/A.php", "second/A.php"] {
        put(
            root,
            path,
            "<?php namespace Demo; class A { static function work() {} }",
        );
    }
    put(
        root,
        "caller.php",
        "<?php function run() { \\Demo\\A::work(); }",
    );
    generated(
        root,
        "",
        "psr4",
        r#"'Demo\\' => array($baseDir . '/first'), 'Dynamic\\' => array(getenv('SOME_PATH')),"#,
    );
    generated(
        root,
        "",
        "classmap",
        r#"'Demo\\A' => $baseDir . '/first/A.php', 'Other' => unknown(),"#,
    );
    let graph = run(&Options::new(root));
    assert!(
        !has_calls(&graph, node(&graph, "caller.php", "run")),
        "{graph}"
    );
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        r#"<?php file_put_contents(__DIR__ . '/executed.txt', 'bad'); return array('Demo\\A' => __DIR__ . '/../../first/A.php');"#,
    );
    let graph = run(&Options::new(root));
    assert!(!has_calls(&graph, node(&graph, "caller.php", "run")));
    assert!(!root.join("vendor/composer/executed.txt").exists());
}

#[test]
fn nearest_projects_are_isolated_and_dependency_manifests_do_not_override_root_loader() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Demo\\":"src/"}}}"#,
    );
    put(
        root,
        "nested/composer.json",
        r#"{"type":"project","autoload":{"psr-4":{"Demo\\":"src/"}}}"#,
    );
    for prefix in ["", "nested/"] {
        put(
            root,
            &format!("{prefix}src/A.php"),
            "<?php namespace Demo; class A { static function work() {} }",
        );
        put(
            root,
            &format!("{prefix}caller.php"),
            "<?php function run() { \\Demo\\A::work(); }",
        );
    }
    put(
        root,
        "nested/src/OnlyNested.php",
        "<?php class OnlyNested { static function work() {} }",
    );
    put(
        root,
        "outside.php",
        "<?php function outside() { OnlyNested::work(); }",
    );
    put(
        root,
        "vendor/pkg/composer.json",
        r#"{"autoload":{"psr-4":{"Demo\\":"decoy/"}}}"#,
    );
    put(
        root,
        "vendor/pkg/caller.php",
        "<?php function package_caller() { \\Demo\\A::work(); }",
    );
    let graph = run(&Options::new(root));
    for prefix in ["", "nested/"] {
        assert!(linked(
            &graph,
            node(&graph, &format!("{prefix}caller.php"), "run"),
            node(&graph, &format!("{prefix}src/A.php"), "Demo\\A::work"),
            "calls"
        ));
    }
    assert!(!has_calls(&graph, node(&graph, "outside.php", "outside")));
    assert!(linked(
        &graph,
        node(&graph, "vendor/pkg/caller.php", "package_caller"),
        node(&graph, "src/A.php", "Demo\\A::work"),
        "calls"
    ));
}

#[test]
fn literal_percent_and_underscore_in_paths_are_not_sql_wildcards() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    for project in ["app_one", "appXone", "app%one", "appZZone"] {
        put(
            root,
            &format!("{project}/composer.json"),
            r#"{"config":{"vendor-dir":"deps_%"},"autoload":{"psr-4":{"Demo\\":"src/"}}}"#,
        );
        put(
            root,
            &format!("{project}/src/A.php"),
            "<?php namespace Demo; class A { static function work() {} }",
        );
        put(
            root,
            &format!("{project}/caller.php"),
            "<?php function run() { \\Demo\\A::work(); }",
        );
    }
    // The second directory is not beneath the first project's literal vendor.
    put(
        root,
        "app_one/deps_foo/composer.json",
        r#"{"autoload":{"psr-4":{"Demo\\":"src/"}}}"#,
    );
    put(
        root,
        "app_one/deps_foo/src/A.php",
        "<?php namespace Demo; class A { static function work() {} }",
    );
    put(
        root,
        "app_one/deps_foo/caller.php",
        "<?php function run() { \\Demo\\A::work(); }",
    );
    let graph = run(&Options::new(root));
    for project in [
        "app_one",
        "appXone",
        "app%one",
        "appZZone",
        "app_one/deps_foo",
    ] {
        assert!(
            linked(
                &graph,
                node(&graph, &format!("{project}/caller.php"), "run"),
                node(&graph, &format!("{project}/src/A.php"), "Demo\\A::work"),
                "calls"
            ),
            "wrong project context for {project}"
        );
    }
}

#[test]
fn missing_fqns_and_stale_classmaps_never_bind_an_unrelated_same_leaf() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    generated(
        root,
        "",
        "classmap",
        r#"'League\\Flysystem\\Config' => $baseDir . '/app/Config.php',"#,
    );
    put(
        root,
        "app/Config.php",
        "<?php namespace Magento\\AwsS3\\Model; class Config { static function work() {} }",
    );
    put(
        root,
        "caller.php",
        "<?php use League\\Flysystem\\Config; function run() { Config::work(); new Config; }",
    );
    let graph = run(&Options::new(root));
    let caller = node(&graph, "caller.php", "run");
    assert!(!has_calls(&graph, caller));
    assert!(!linked(
        &graph,
        caller,
        node(&graph, "app/Config.php", "Magento\\AwsS3\\Model\\Config"),
        "instantiates"
    ));
    // The mapped indexed file contradicts the FQN. Do not silently select a
    // different implementation merely because it happens to be globally unique.
    put(
        root,
        "other/Config.php",
        "<?php namespace League\\Flysystem; class Config { static function work() {} }",
    );
    let graph = run(&Options::new(root));
    assert!(!has_calls(&graph, node(&graph, "caller.php", "run")));
}

#[test]
fn parent_calls_follow_actual_parent_chain_and_ignore_child_overrides_and_trait_contexts() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "hierarchy.php",
        r#"<?php
class Grand { function __construct() {} function work() {} }
class Middle extends Grand {}
trait OverrideWork { function work() {} }
class Child extends Middle {
 use OverrideWork;
 function __construct() { parent::__construct(); }
 function work() { parent::work(); }
 function run() { parent::work(); }
}
trait DeferredParent { function deferred() { parent::work(); } }
class UsesDeferred extends Middle { use DeferredParent; }
class Missing extends Absent { function run() { parent::work(); } }
class CycleA extends CycleB { function run() { parent::absent(); } }
class CycleB extends CycleA {}
class Dynamic extends Middle { function run() { static::work(); } }
"#,
    );
    let graph = run(&Options::new(d.path()));
    for (source, target) in [
        ("Child::__construct", "Grand::__construct"),
        ("Child::work", "Grand::work"),
        ("Child::run", "Grand::work"),
    ] {
        assert!(linked(
            &graph,
            node(&graph, "hierarchy.php", source),
            node(&graph, "hierarchy.php", target),
            "calls"
        ));
    }
    for source in [
        "DeferredParent::deferred",
        "Missing::run",
        "CycleA::run",
        "Dynamic::run",
    ] {
        assert!(
            !has_calls(&graph, node(&graph, "hierarchy.php", source)),
            "unsafe parent/static call from {source}"
        );
    }
    assert!(!linked(
        &graph,
        node(&graph, "hierarchy.php", "Child::run"),
        node(&graph, "hierarchy.php", "OverrideWork::work"),
        "calls"
    ));
}

#[test]
fn ambiguous_parent_fqn_does_not_fall_back_to_child_method() {
    let d = TempDir::new().unwrap();
    for file in ["a.php", "b.php"] {
        put(d.path(), file, "<?php class Base { function work() {} }");
    }
    put(
        d.path(),
        "child.php",
        "<?php class Child extends Base { function work() { parent::work(); } }",
    );
    let graph = run(&Options::new(d.path()));
    assert!(!has_calls(&graph, node(&graph, "child.php", "Child::work")));
}

#[test]
fn byte_identifiers_in_generated_classmap_select_the_correct_duplicate_without_collisions() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    put(root, "composer.json", "{}");
    let mut source = b"<?php class Class".to_vec();
    source.push(0xa9);
    source.extend_from_slice(b" { static function work() {} }");
    put(root, "chosen.php", &source);
    put(root, "other.php", &source);
    let mut caller = b"<?php function run() { Class".to_vec();
    caller.push(0xa9);
    caller.extend_from_slice(b"::work(); }");
    put(root, "caller.php", caller);
    let mut map = b"<?php return array('Class".to_vec();
    map.push(0xa9);
    map.extend_from_slice(b"' => __DIR__ . '/../../chosen.php');");
    put(root, "vendor/composer/autoload_classmap.php", map);
    let graph = run(&Options::new(root));
    let source = node(&graph, "caller.php", "run");
    let chosen = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == "chosen.php" && n["type"] == "method")
        .unwrap();
    let other = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == "other.php" && n["type"] == "method")
        .unwrap();
    assert!(linked(&graph, source, chosen, "calls"));
    assert!(!linked(&graph, source, other, "calls"));
}

#[test]
fn reserved_encoding_marker_in_json_prefix_and_generated_paths_preserves_file_identity() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    let marker = '\u{f100}';
    let namespace = format!("Demo{marker}");
    let source = format!("<?php namespace {namespace}; class A {{ static function work() {{}} }}");
    let chosen = format!("src{marker}/A.php");
    put(root, &chosen, &source);
    put(root, "other/A.php", &source);
    put(
        root,
        "caller.php",
        format!("<?php function run() {{ \\{namespace}\\A::work(); }}"),
    );
    put(root,"composer.json",serde_json::to_vec(&serde_json::json!({"autoload":{"psr-4":{format!("{namespace}\\"):format!("src{marker}/")}}})).unwrap());
    let graph = run(&Options::new(root));
    let target = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == chosen && n["type"] == "method")
        .unwrap();
    assert!(linked(
        &graph,
        node(&graph, "caller.php", "run"),
        target,
        "calls"
    ));
    // Generated PHP map paths are filesystem strings; only the identifier key
    // retains the graph's reversible source-byte encoding.
    put(root, "composer.json", "{}");
    generated(
        root,
        "",
        "classmap",
        &format!("'{namespace}\\\\A' => $baseDir . '/src{marker}/A.php',"),
    );
    let graph = run(&Options::new(root));
    let target = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == chosen && n["type"] == "method")
        .unwrap();
    assert!(linked(
        &graph,
        node(&graph, "caller.php", "run"),
        target,
        "calls"
    ));
}

#[test]
fn psr4_filename_with_reserved_encoding_marker_matches_the_actual_path() {
    let d = TempDir::new().unwrap();
    let root = d.path();
    let class = "Thing\u{f100}";
    let source = format!("<?php namespace Demo; class {class} {{ static function work() {{}} }}");
    let chosen = format!("src/{class}.php");
    put(root, &chosen, &source);
    put(root, "other.php", &source);
    put(
        root,
        "caller.php",
        format!("<?php function run() {{ \\Demo\\{class}::work(); }}"),
    );
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Demo\\":"src/"}}}"#,
    );
    let graph = run(&Options::new(root));
    let target = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == chosen && n["type"] == "method")
        .unwrap();
    assert!(linked(
        &graph,
        node(&graph, "caller.php", "run"),
        target,
        "calls"
    ));
}
