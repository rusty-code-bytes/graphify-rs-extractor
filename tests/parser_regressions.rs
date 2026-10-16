use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: impl AsRef<[u8]>) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, source).unwrap();
}
fn graph(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("graphify-out/graph.json")).unwrap()).unwrap()
}
fn node<'a>(graph: &'a Value, kind: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["type"] == kind && n["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label}: {graph}"))
}
fn calls(graph: &Value, source: &Value, target: &Value) -> bool {
    graph["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == "calls"
    })
}

#[test]
fn short_echo_comma_lists_work_at_start_and_inside_html() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "defs.php",
        "<?php function first() {} function second() {} function decoy() {}",
    );
    put(
        root,
        "start.phtml",
        "<?= first(), /* Magento noEscape */ second() ?>",
    );
    put(
        root,
        "inline.phtml",
        "<?php ?>\n<button <?= first(), second() ?>><?= first() ?></button>",
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    for path in ["start.phtml", "inline.phtml"] {
        let caller = node(&g, "file", path);
        assert!(calls(&g, caller, node(&g, "function", "first")));
        assert!(calls(&g, caller, node(&g, "function", "second")));
        assert!(!calls(&g, caller, node(&g, "function", "decoy")));
    }
}

#[test]
fn boolean_class_constant_names_leave_methods_and_calls_intact() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "constants.php",
        r#"<?php
class Config { const TRUE = 'true'; const FALSE = 'false'; public static function read() {} }
class ConstraintError { const FALSE = 'false'; public static function read() {} }
function run() { Config::read(); }
"#,
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let run = node(&g, "function", "run");
    assert!(calls(&g, run, node(&g, "method", "Config::read")));
    assert!(!calls(&g, run, node(&g, "method", "ConstraintError::read")));
}

#[test]
fn readonly_promoted_parameters_without_visibility_keep_type_identity() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "readonly.php",
        r#"<?php
class Target { function ping() {} } class Other { function ping() {} }
class SortableReferenceObject {
    function __construct(readonly Target $reference, readonly int $sort) { $reference->ping(); }
    function after() { Target::ping(); }
}
"#,
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let constructor = node(&g, "method", "SortableReferenceObject::__construct");
    assert!(calls(&g, constructor, node(&g, "method", "Target::ping")));
    assert!(!calls(&g, constructor, node(&g, "method", "Other::ping")));
    assert!(calls(
        &g,
        node(&g, "method", "SortableReferenceObject::after"),
        node(&g, "method", "Target::ping")
    ));
}

#[test]
fn trailing_commas_in_class_function_and_mixed_group_imports_preserve_aliases() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "groups.php",
        r#"<?php
namespace Lib { class A { static function go() {} } class B { static function go() {} } function assist() {} }
namespace App {
    use Lib\{A as Renamed, B,};
    use function Lib\{assist as help,};
    function run() { Renamed::go(); help(); }
}
namespace Mixed {
    use Lib\{A as Renamed, function assist as help,};
    function run() { Renamed::go(); help(); }
}
"#,
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    for name in ["App\\run", "Mixed\\run"] {
        let caller = node(&g, "function", name);
        assert!(calls(&g, caller, node(&g, "method", "Lib\\A::go")));
        assert!(calls(&g, caller, node(&g, "function", "Lib\\assist")));
        assert!(!calls(&g, caller, node(&g, "method", "Lib\\B::go")));
    }
}

#[test]
fn casts_of_match_expressions_visit_condition_and_all_arms() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "match.php",
        r#"<?php
function condition() {} function arm() {} function fallback() {} function decoy() {}
function convert() { return (string) match (condition()) { 1 => arm(), default => fallback(), }; }
"#,
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let convert = node(&g, "function", "convert");
    for target in ["condition", "arm", "fallback"] {
        assert!(calls(&g, convert, node(&g, "function", target)));
    }
    assert!(!calls(&g, convert, node(&g, "function", "decoy")));
}

#[test]
fn long_concatenations_walk_all_calls_and_leave_scope_intact() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for (file, terms) in [("Biz.php", 712), ("Cn.php", 310), ("Jp.php", 3000)] {
        let mut parts = vec!["'fake()'"; terms];
        parts[0] = "first()";
        parts[terms / 2] = "middle()";
        parts[terms - 1] = "last()";
        put(
            root,
            file,
            format!(
                "<?php namespace {}; function first() {{}} function middle() {{}} function last() {{}} function fake() {{}} function long() {{ return {}; }} function after() {{ last(); }}",
                file.trim_end_matches(".php"),
                parts.join(" . ")
            ),
        );
    }
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    for namespace in ["Biz", "Cn", "Jp"] {
        let long = node(&g, "function", &format!("{namespace}\\long"));
        for name in ["first", "middle", "last"] {
            assert!(calls(
                &g,
                long,
                node(&g, "function", &format!("{namespace}\\{name}"))
            ));
        }
        assert!(!calls(
            &g,
            long,
            node(&g, "function", &format!("{namespace}\\fake"))
        ));
        assert!(calls(
            &g,
            node(&g, "function", &format!("{namespace}\\after")),
            node(&g, "function", &format!("{namespace}\\last"))
        ));
    }
}

#[test]
fn invalid_utf8_php_identifier_bytes_are_distinct_from_unicode_and_reserved_escape() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let names: Vec<Vec<u8>> = vec![
        vec![0xa9],
        vec![0xaa],
        "©".as_bytes().to_vec(),
        "�".as_bytes().to_vec(),
        "\u{f100}\u{e0a9}".as_bytes().to_vec(),
    ];
    for (i, name) in names.iter().enumerate() {
        let mut source = b"<?php namespace Symfony\\Component\\Cache\\Traits; class ".to_vec();
        source.extend(name);
        source.extend(b" { static function save() {} }");
        put(root, &format!("value{i}.php"), source);
        let mut source =
            b"<?php namespace Symfony\\Component\\Cache\\Traits; function caller".to_vec();
        source.extend(i.to_string().as_bytes());
        source.extend(b"() { ");
        source.extend(name);
        source.extend(b"::save(); }");
        put(root, &format!("caller{i}.php"), source);
    }
    // Generated Composer maps contain the same invalid byte inside a string.
    let mut map = b"<?php return ['Symfony\\\\Component\\\\Cache\\\\Traits\\\\".to_vec();
    map.push(0xa9);
    map.extend(b"' => __DIR__ . '/value0.php'];");
    put(root, "autoload_classmap.php", &map);
    put(root, "autoload_static.php", map);
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let methods: Vec<_> = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "method")
        .collect();
    assert_eq!(methods.len(), names.len());
    let mut targets = std::collections::HashSet::new();
    for i in 0..names.len() {
        let caller = node(
            &g,
            "function",
            &format!("Symfony\\Component\\Cache\\Traits\\caller{i}"),
        );
        let outgoing: Vec<_> = g["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source"] == caller["id"] && e["relation"] == "calls")
            .collect();
        assert_eq!(outgoing.len(), 1);
        let expected = methods
            .iter()
            .find(|m| m["source_file"] == format!("value{i}.php"))
            .unwrap();
        assert_eq!(outgoing[0]["target"], expected["id"]);
        assert!(targets.insert(outgoing[0]["target"].as_str().unwrap()));
    }
}

#[test]
fn hack_extension_fixtures_are_transparently_inventoried_for_each_worker_mode() {
    for workers in [1, 8] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for (path, source) in [
            (
                "all_options.php",
                "<?hh\nnamespace Fixtures; function all_options() { dispatcher($collector ==> {}, shape('cacheDisabled' => false)); }",
            ),
            (
                "empty_options.php",
                "\u{feff}<?hh\nfunction empty_options() { dispatcher($collector ==> {}, shape()); }",
            ),
            (
                "no_options.php",
                " \n<?hh\nfunction no_options() { dispatcher($collector ==> {}); }",
            ),
        ] {
            put(root, path, source);
        }
        put(root, "real.php", "<?php function real() { $s = '<?hh'; }");
        let mut options = Options::new(root);
        options.workers = workers;
        let stats = extract(&options).unwrap();
        assert_eq!(stats.unsupported_hack_files, 3);
        assert_eq!(stats.files, 4);
        let g = graph(root);
        for path in ["all_options.php", "empty_options.php", "no_options.php"] {
            let file = node(&g, "file", path);
            assert_eq!(file["language"], "hack");
            assert_eq!(file["_origin"], "inventory");
        }
        assert_eq!(
            g["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| n["type"] == "function")
                .count(),
            1
        );
        node(&g, "function", "real");
    }
}

#[test]
fn grammar_fixes_keep_malformed_php_fail_closed() {
    for source in [
        "<?php function first() {} first(), second();",
        "<?= first(), ?>",
        "<?php class Missing { function broken( {}",
        "<?php function real() {} (string) match (true) { default => ; };",
    ] {
        let dir = TempDir::new().unwrap();
        put(dir.path(), "broken.php", source);
        assert!(
            extract(&Options::new(dir.path())).is_err(),
            "accepted malformed PHP: {source}"
        );
    }
}

#[test]
fn reserved_encoding_marker_preserves_filesystem_paths_and_distinct_aliases() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let marker = '\u{f100}';
    let folder = format!("sub{marker}");
    let caller_path = format!("{folder}/caller.php");
    let target_path = format!("{folder}/target{marker}.php");
    let decoy_path = format!("{folder}/target{marker}{marker}.php");
    put(root, &target_path, "<?php function target() {}");
    put(root, &decoy_path, "<?php function decoy() {}");
    put(
        root,
        "defs.php",
        "<?php namespace Lib; class Actual { static function run() {} } class Decoy { static function run() {} }",
    );
    put(
        root,
        &caller_path,
        format!(
            "<?php\nnamespace App;\nuse Lib\\Actual as Alias{marker};\nuse Lib\\Decoy as Alias{marker}{marker};\nfunction run() {{ Alias{marker}::run(); }}\ninclude __DIR__ . '/target{marker}.php';\ninclude __FILE__;\n?>\n<a href=\"target{marker}.php\">valid target</a>\n"
        ),
    );
    // Invalid identifier bytes inside a path must not become a valid replacement
    // or private-use filename. No lossy or encoded-path target is invented.
    let mut invalid = b"<?php include __DIR__ . '/target".to_vec();
    invalid.push(0xa9);
    invalid.extend(b".php';");
    put(root, &format!("{folder}/invalid.php"), invalid);
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let file = node(&g, "file", &caller_path);
    let target = node(&g, "file", &target_path);
    let decoy = node(&g, "file", &decoy_path);
    for relation in ["includes", "links_to"] {
        let has = |destination: &Value| {
            g["links"].as_array().unwrap().iter().any(|e| {
                e["source"] == file["id"]
                    && e["target"] == destination["id"]
                    && e["relation"] == relation
            })
        };
        assert!(
            has(target),
            "missing {relation} to original filesystem path"
        );
        assert!(!has(decoy));
    }
    assert!(
        g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == file["id"]
                && e["target"] == file["id"]
                && e["relation"] == "includes")
    );
    let run = node(&g, "function", "App\\run");
    assert!(calls(&g, run, node(&g, "method", "Lib\\Actual::run")));
    assert!(!calls(&g, run, node(&g, "method", "Lib\\Decoy::run")));
    let invalid = node(&g, "file", &format!("{folder}/invalid.php"));
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == invalid["id"] && e["relation"] == "includes")
    );
}
