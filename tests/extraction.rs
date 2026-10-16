use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}
fn graph(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("graphify-out/graph.json")).unwrap()).unwrap()
}
fn find<'a>(g: &'a Value, kind: &str, label: &str) -> &'a Value {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["type"] == kind && n["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label} in {g}"))
}
fn edge(g: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    g["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == relation
    })
}

#[test]
fn php_scopes_aliases_and_global_cross_file_resolution() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "lib.php",
        r#"<?php
namespace Library;
class A {
    public static function save() {}
}
class B {
    public static function save() {}
}
function helper() {}
"#,
    );
    put(
        root,
        "caller.php",
        r#"<?php
namespace App;
use Library\A as Renamed;
use function Library\helper as assist;
function run() {
    Renamed::save();
    assist();
    new \Library\B();
}
"#,
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let a = find(&g, "method", "Library\\A::save");
    let b = find(&g, "method", "Library\\B::save");
    assert_ne!(a["id"], b["id"]);
    let run = find(&g, "function", "App\\run");
    assert!(edge(&g, run, a, "calls"));
    assert!(!edge(&g, run, b, "calls"));
    assert!(edge(
        &g,
        run,
        find(&g, "function", "Library\\helper"),
        "calls"
    ));
    assert!(edge(
        &g,
        run,
        find(&g, "class", "Library\\B"),
        "instantiates"
    ));
    assert!(edge(&g, find(&g, "class", "Library\\A"), a, "defines"));
}

#[test]
fn php_bracketed_namespaces_group_imports_and_global_fallback() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "all.php",
        r#"<?php
namespace { function shared() {} }
namespace Lib { class A {} class B {} function assist() {} }
namespace App {
    use Lib\{A as X, B};
    use function Lib\{assist as help};
    function run() { new X(); new B(); help(); shared(); }
}
"#,
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let run = find(&g, "function", "App\\run");
    assert!(edge(&g, run, find(&g, "class", "Lib\\A"), "instantiates"));
    assert!(edge(&g, run, find(&g, "class", "Lib\\B"), "instantiates"));
    assert!(edge(&g, run, find(&g, "function", "Lib\\assist"), "calls"));
    assert!(edge(&g, run, find(&g, "function", "shared"), "calls"));
}

#[test]
fn ambiguous_definitions_do_not_get_arbitrary_call_edges() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(root, "a.php", "<?php function duplicate() {}\n");
    put(root, "b.php", "<?php function duplicate() {}\n");
    put(root, "c.php", "<?php function caller() { duplicate(); }\n");
    let stats = extract(&Options::new(root)).unwrap();
    let g = graph(root);
    assert_eq!(stats.unresolved_references, 1);
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relation"] == "calls")
    );
}

#[test]
fn php_comments_strings_closures_and_dynamic_calls() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "a.php",
        r#"<?php
// function fake() {}
$s = 'class Phantom { function fake() {} }';
function real() {}
function outer() { $f = function() { real(); }; $obj->real(); }
"#,
    );
    let stats = extract(&Options::new(root)).unwrap();
    let g = graph(root);
    assert_eq!(stats.dynamic_php_references, 1);
    assert!(
        !g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "fake" || n["label"] == "Phantom")
    );
    assert!(!edge(
        &g,
        find(&g, "function", "outer"),
        find(&g, "function", "real"),
        "calls"
    ));
}

#[test]
fn sql_schema_foreign_keys_migrations_and_dump_payload() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "schema.sql",
        "CREATE TABLE users(id INT PRIMARY KEY); CREATE TABLE posts(id INT, user_id INT, FOREIGN KEY(user_id) REFERENCES users(id));",
    );
    put(
        root,
        "migration.sql",
        "ALTER TABLE posts ADD CONSTRAINT fk FOREIGN KEY(user_id) REFERENCES users(id);",
    );
    put(
        root,
        "dump.sql",
        &format!("INSERT INTO users VALUES ('{};');", "x".repeat(2_000_000)),
    );
    let mut options = Options::new(root);
    options.max_file_mib = 1;
    let stats = extract(&options).unwrap();
    let g = graph(root);
    assert_eq!(stats.skipped_sql_data_statements, 1);
    assert!(edge(
        &g,
        find(&g, "table", "posts"),
        find(&g, "table", "users"),
        "references"
    ));
    assert!(edge(
        &g,
        find(&g, "migration", "ALTER posts@1"),
        find(&g, "table", "posts"),
        "alters"
    ));
}

#[test]
fn postgres_copy_and_quoted_identifiers_remain_distinct() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "a.sql",
        "CREATE TABLE foo(id INT); CREATE TABLE \"Foo\"(id INT); CREATE TABLE child(id INT REFERENCES \"Foo\"(id)); COPY foo FROM STDIN;\n1\n\\.\n",
    );
    let mut options = Options::new(root);
    options.sql_dialect = "postgres".into();
    extract(&options).unwrap();
    let g = graph(root);
    assert!(edge(
        &g,
        find(&g, "table", "child"),
        find(&g, "table", "\"Foo\""),
        "references"
    ));
    assert!(!edge(
        &g,
        find(&g, "table", "child"),
        find(&g, "table", "foo"),
        "references"
    ));
}

#[test]
fn html_links_forms_and_mixed_php_html() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "pages/index.html",
        r#"<a href="../handler.php?x=1&amp;y=2">go</a><form action="/handler.php"></form>"#,
    );
    put(
        root,
        "handler.php",
        r#"<?php $s = '<a href="fake.php">'; ?><a href="pages/index.html">Back</a>"#,
    );
    put(root, "fake.php", "<?php");
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    let index = find(&g, "file", "pages/index.html");
    let handler = find(&g, "file", "handler.php");
    assert!(edge(&g, index, handler, "links_to"));
    assert!(edge(&g, index, handler, "submits_to"));
    assert!(edge(&g, handler, index, "links_to"));
    assert!(!edge(&g, handler, find(&g, "file", "fake.php"), "links_to"));
}

#[test]
fn exclusions_happen_before_parsing_and_rebuild_removes_old_nodes() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(root, "keep.php", "<?php function keep() {}");
    put(root, "old.php", "<?php function old() {}");
    extract(&Options::new(root)).unwrap();
    put(root, "old.php", "<?php BROKEN {");
    put(root, "backups/dump.sql", "NOT VALID SQL");
    put(root, "literal[1].php", "<?php BROKEN {");
    let patterns = dir.path().join("excludes.txt");
    fs::write(&patterns, "# root-relative\nbackups/**\n").unwrap();
    let mut options = Options::new(root);
    options.exclude_from.push(patterns);
    options.exclude_file.push("old.php".into());
    options.exclude_file.push("literal[1].php".into());
    extract(&options).unwrap();
    let g = graph(root);
    assert!(
        !g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["source_file"] == "old.php")
    );
    assert_eq!(g["graph"]["stats"]["files"], 1);
}

#[test]
fn failure_preserves_existing_graph_and_retry_really_rebuilds() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(root, "a.php", "<?php function old() {}");
    let options = Options::new(root);
    extract(&options).unwrap();
    let old = fs::read(root.join("graphify-out/graph.json")).unwrap();
    put(root, "a.php", "<?php function broken( {");
    assert!(extract(&options).is_err());
    assert_eq!(old, fs::read(root.join("graphify-out/graph.json")).unwrap());
    put(root, "a.php", "<?php function repaired() {}");
    extract(&options).unwrap();
    find(&graph(root), "function", "repaired");
}

#[test]
fn invalid_exclusions_and_oversized_files_fail_closed() {
    let dir = TempDir::new().unwrap();
    let mut options = Options::new(dir.path());
    options.exclude.push("[".into());
    assert!(extract(&options).is_err());
    options.exclude.clear();
    options.exclude_from.push(dir.path().join("missing.txt"));
    assert!(extract(&options).is_err());
    options.exclude_from.clear();
    options.max_file_mib = 1;
    put(dir.path(), "large.html", &"x".repeat(1_048_577));
    assert!(extract(&options).is_err());
}

#[test]
fn deterministic_output_and_ids_independent_of_checkout_location() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    for root in [a.path(), b.path()] {
        put(root, "x/a.php", "<?php function demo() {}\n");
        extract(&Options::new(root)).unwrap();
    }
    let first = fs::read(a.path().join("graphify-out/graph.json")).unwrap();
    assert_eq!(
        first,
        fs::read(b.path().join("graphify-out/graph.json")).unwrap()
    );
    extract(&Options::new(a.path())).unwrap();
    assert_eq!(
        first,
        fs::read(a.path().join("graphify-out/graph.json")).unwrap()
    );
}

#[test]
fn every_edge_has_endpoints_and_paths_are_relative() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "a.php", "<?php function f() { absent(); }\n");
    extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    let nodes = g["nodes"].as_array().unwrap();
    for n in nodes {
        assert!(!Path::new(n["source_file"].as_str().unwrap()).is_absolute());
    }
    for e in g["links"].as_array().unwrap() {
        for end in ["source", "target"] {
            assert!(nodes.iter().any(|n| n["id"] == e[end]));
        }
    }
}

#[test]
fn php_inheritance_interfaces_and_traits_are_explicit_edges() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "a.php",
        "<?php namespace App;\ninterface Contract {}\ntrait Helpers {}\nclass Base {}\nclass Child extends Base implements Contract { use Helpers; }\n",
    );
    extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    let child = find(&g, "class", "App\\Child");
    assert!(edge(&g, child, find(&g, "class", "App\\Base"), "extends"));
    assert!(edge(
        &g,
        child,
        find(&g, "interface", "App\\Contract"),
        "implements"
    ));
    assert!(edge(
        &g,
        child,
        find(&g, "trait", "App\\Helpers"),
        "uses_trait"
    ));
}

#[test]
fn mysql_qualified_tables_and_executable_comments() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "a.sql",
        "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\nCREATE TABLE app.users(id INT PRIMARY KEY); CREATE TABLE app.posts(uid INT REFERENCES users(id));",
    );
    extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    assert!(edge(
        &g,
        find(&g, "table", "app.posts"),
        find(&g, "table", "app.users"),
        "references"
    ));
}

#[test]
fn html_base_and_php_dynamic_urls_do_not_fabricate_targets() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "base.html",
        "<base href='https://example.com/'><a href='/target.php'>external</a>",
    );
    put(
        dir.path(),
        "dynamic.php",
        "<?php $url = 'target'; ?><a href=\"<?php echo $url; ?>.php\">dynamic</a>",
    );
    put(dir.path(), "target.php", "<?php");
    let stats = extract(&Options::new(dir.path())).unwrap();
    assert!(stats.skipped_html_references >= 2);
    assert!(graph(dir.path())["links"].as_array().unwrap().is_empty());
}

#[test]
fn cli_patterns_use_project_root_not_working_directory() {
    let dir = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    put(dir.path(), "a.php", "<?php function keep() {}");
    put(dir.path(), "nested/broken.php", "<?php BROKEN {");
    fs::write(elsewhere.path().join("exclude.txt"), "nested/**\n").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_graphify-extractor"))
        .current_dir(elsewhere.path())
        .args([
            "extract",
            dir.path().to_str().unwrap(),
            "--exclude-from",
            "exclude.txt",
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(graph(dir.path())["graph"]["stats"]["files"], 1);
}

#[test]
fn gitignore_and_explicit_exclusions_are_independent() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), ".gitignore", "ignored.php\n");
    put(dir.path(), "ignored.php", "<?php BROKEN {");
    put(dir.path(), "a.php", "<?php function keep() {}");
    let mut options = Options::new(dir.path());
    extract(&options).unwrap();
    options.no_gitignore = true;
    assert!(extract(&options).is_err());
    options.exclude.push("ignored.php".into());
    extract(&options).unwrap();
}

#[cfg(unix)]
#[test]
fn symlink_cycles_and_external_targets_are_not_followed() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    put(outside.path(), "broken.php", "<?php BROKEN {");
    std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("broken.php"),
        dir.path().join("alias.php"),
    )
    .unwrap();
    put(dir.path(), "a.php", "<?php function keep() {}");
    assert_eq!(extract(&Options::new(dir.path())).unwrap().files, 1);
}

#[test]
fn this_and_readonly_typed_receivers_are_inferred_not_certain() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "types.php",
        r#"<?php namespace Lib;
class Service { public function save() {} }
"#,
    );
    put(
        dir.path(),
        "caller.php",
        r#"<?php namespace App;
use Lib\Service as Store;
class Worker {
    public function save() {}
    public function run(Store $store) { $store->save(); $this->save(); }
    public static function staticRun() { $this->save(); }
}
function nullable(?Store $store) { $store?->save(); }
"#,
    );
    let stats = extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    assert_eq!(stats.inferred_edges, 3);
    let run = find(&g, "method", "App\\Worker::run");
    assert!(edge(
        &g,
        run,
        find(&g, "method", "Lib\\Service::save"),
        "calls"
    ));
    assert!(edge(
        &g,
        run,
        find(&g, "method", "App\\Worker::save"),
        "calls"
    ));
    for e in g["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
    {
        assert_eq!(e["confidence"], "INFERRED");
        assert_eq!(e["confidence_score"], 0.7);
    }
    assert!(!g["links"].as_array().unwrap().iter().any(|e| e["source"]
        == find(&g, "method", "App\\Worker::staticRun")["id"]
        && e["relation"] == "calls"));
}

#[test]
fn reassigned_escaped_byref_variadic_and_union_parameters_are_not_guessed() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "a.php",
        r#"<?php
class Service { function save() {} }
class Other { function save() {} }
function reassigned(Service $s) { $s = new Other; $s->save(); }
function escaped(Service $s) { unknown($s); $s->save(); }
function byref(Service &$s) { $s->save(); }
function variadic(Service ...$s) { $s->save(); }
function unioned(Service|Other $s) { $s->save(); }
function dynamic(Service $s) { extract([]); $s->save(); }
"#,
    );
    let stats = extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    assert_eq!(stats.inferred_edges, 1);
    assert!(edge(
        &g,
        find(&g, "function", "reassigned"),
        find(&g, "method", "Other::save"),
        "calls"
    ));
    assert!(!edge(
        &g,
        find(&g, "function", "reassigned"),
        find(&g, "method", "Service::save"),
        "calls"
    ));
    assert_eq!(
        g["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .count(),
        1
    );
}

#[test]
fn explicit_call_evidence_wins_when_an_inferred_edge_has_same_endpoints() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "a.php",
        r#"<?php
class Service {
    public static function save() {}
    public function run() { $this->save(); self::save(); }
}
"#,
    );
    let stats = extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    assert_eq!(stats.inferred_edges, 0);
    let calls: Vec<_> = g["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["confidence"], "EXTRACTED");
}

#[test]
fn anchored_includes_resolve_without_guessing_runtime_working_directory() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "src/entry.php",
        r#"<?php
require_once __DIR__ . '/../lib/helper.php';
include (__DIR__ . '/local.php');
include 'local.php';
include __DIR__ . '/../../outside.php';
"#,
    );
    put(dir.path(), "lib/helper.php", "<?php function helper() {}");
    put(dir.path(), "src/local.php", "<?php");
    let stats = extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    let entry = find(&g, "file", "src/entry.php");
    assert!(edge(
        &g,
        entry,
        find(&g, "file", "lib/helper.php"),
        "includes"
    ));
    assert!(edge(
        &g,
        entry,
        find(&g, "file", "src/local.php"),
        "includes"
    ));
    assert_eq!(stats.dynamic_php_references, 2);
}

#[test]
fn graphify_context_filters_keep_calls_and_imports_separate() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "lib.php",
        "<?php namespace Lib; class Service {} function target() {}\n",
    );
    put(
        dir.path(),
        "main.php",
        r#"<?php
use Lib\Service;
use function Lib\target;
require __DIR__ . '/lib.php';
function caller() { target(); new Service(); }
"#,
    );
    extract(&Options::new(dir.path())).unwrap();
    let g = graph(dir.path());
    let edges = g["links"].as_array().unwrap();
    for relation in ["calls", "instantiates", "imports", "includes"] {
        let matching: Vec<_> = edges.iter().filter(|e| e["relation"] == relation).collect();
        assert!(!matching.is_empty(), "fixture must exercise {relation}");
        let context = if matches!(relation, "calls" | "instantiates") {
            "call"
        } else {
            "import"
        };
        assert!(matching.iter().all(|e| e["context"] == context));
    }
    assert!(
        edges
            .iter()
            .filter(|e| e["relation"] == "defines")
            .all(|e| e.get("context").is_none())
    );
}

#[test]
fn duplicate_stub_does_not_erase_lexically_owned_methods() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "runtime.php",
        "<?php function help() {} class Client { function save() {} function run() {$this->save(); self::save(); help();} }",
    );
    extract(&Options::new(d.path())).unwrap();
    let before = graph(d.path());
    put(
        d.path(),
        "stub.php",
        "<?php function help() {} class Client { function save() {} function run() {} }",
    );
    put(
        d.path(),
        "external.php",
        "<?php function outside() {Client::save(); help();}",
    );
    extract(&Options::new(d.path())).unwrap();
    let after = graph(d.path());
    let run = find(&before, "method", "Client::run");
    let save = find(&before, "method", "Client::save");
    assert!(edge(&after, run, save, "calls"));
    assert!(edge(
        &after,
        run,
        find(&before, "function", "help"),
        "calls"
    ));
    let outside = find(&after, "function", "outside");
    assert!(
        !after["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == outside["id"] && e["relation"] == "calls")
    );
}

#[test]
fn receivers_resolve_real_class_and_inherited_method_not_name_collision() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "caller.php",
        r#"<?php
class Product {
 function getNomUrl() {}
 function run() { $thirdparty = new Supplier(); $thirdparty->getNomUrl(); }
}
"#,
    );
    put(
        d.path(),
        "supplier.php",
        "<?php class Supplier extends Company {}",
    );
    put(
        d.path(),
        "company.php",
        "<?php class Company { function getNomUrl() {} }",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    let caller = find(&g, "method", "Product::run");
    assert!(edge(
        &g,
        caller,
        find(&g, "method", "Company::getNomUrl"),
        "calls"
    ));
    assert!(!edge(
        &g,
        caller,
        find(&g, "method", "Product::getNomUrl"),
        "calls"
    ));
}

#[test]
fn receiver_flow_tracks_reassignment_and_invalidates_branch_escape_and_alias() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "a.php",
        r#"<?php
class A { function work() {} }
class B { function work() {} }
function straight() { $v=new A; $v->work(); $v=new B; $v->work(); }
function branch($condition) { $v=new A; if ($condition) { $v=new B; } $v->work(); }
function escaped() { $v=new A; unknown($v); $v->work(); }
function aliasing() { $v=new A; $a=&$v; $a=new B; $v->work(); }
function unset_self() { $v=new A; unset($v); $v->work(); }
function unset_other() { $v=new A; unset($other, $items[0]); $v->work(); }
function loop($items) { $v=new A; foreach ($items as $v) { $v->work(); } $v->work(); }
"#,
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    let straight = find(&g, "function", "straight");
    for class in ["A::work", "B::work"] {
        assert!(edge(&g, straight, find(&g, "method", class), "calls"));
    }
    assert!(edge(
        &g,
        find(&g, "function", "unset_other"),
        find(&g, "method", "A::work"),
        "calls"
    ));
    for name in ["branch", "escaped", "aliasing", "loop", "unset_self"] {
        let caller = find(&g, "function", name);
        assert!(
            !g["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["source"] == caller["id"] && e["relation"] == "calls"),
            "unsafe inference in {name}"
        );
    }
}

#[test]
fn global_receivers_and_top_level_calls_survive_unrelated_loop_assignments() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "a.php",
        r#"<?php
class Form { function select() {} }
$form=new Form;
parse_str("x=1", $query);
render([]);
function render($items) { global $form; foreach ($items as $key => $value) { $form->select(); } }
"#,
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    assert!(edge(
        &g,
        find(&g, "file", "a.php"),
        find(&g, "function", "render"),
        "calls"
    ));
    assert!(edge(
        &g,
        find(&g, "function", "render"),
        find(&g, "method", "Form::select"),
        "calls"
    ));
}

#[test]
fn method_precedence_traits_overrides_cycles_and_new_self() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "a.php",
        r#"<?php
trait T { function save() {} }
class Base { function save() {} }
class Child extends Base { use T; function run() { $this->save(); $x=new self; $x->save(); } }
class OverrideChild extends Child { function save() {} function run() {$this->save();} }
class CycleA extends CycleB { function run() {$this->absent();} }
class CycleB extends CycleA {}
trait U { function save() {} }
class Adapted extends Base { use T,U { T::save insteadof U; } function run() {$this->save();} }
"#,
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    assert!(edge(
        &g,
        find(&g, "method", "Child::run"),
        find(&g, "method", "T::save"),
        "calls"
    ));
    assert!(edge(
        &g,
        find(&g, "method", "Child::run"),
        find(&g, "class", "Child"),
        "instantiates"
    ));
    assert!(edge(
        &g,
        find(&g, "method", "OverrideChild::run"),
        find(&g, "method", "OverrideChild::save"),
        "calls"
    ));
    assert!(!g["links"].as_array().unwrap().iter().any(|e| e["source"]
        == find(&g, "method", "Adapted::run")["id"]
        && e["relation"] == "calls"));
}

#[test]
fn alter_foreign_key_projects_table_edge_with_migration_provenance() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "tables.sql",
        "CREATE TABLE child (id INT); CREATE TABLE parent (id INT);",
    );
    put(
        d.path(),
        "migration.sql",
        "ALTER TABLE child ADD FOREIGN KEY (id) REFERENCES parent(id);",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    let child = find(&g, "table", "child");
    let parent = find(&g, "table", "parent");
    assert!(edge(&g, child, parent, "references"));
    let relation = g["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["source"] == child["id"]
                && e["target"] == parent["id"]
                && e["relation"] == "references"
        })
        .unwrap();
    assert_eq!(relation["source_file"], "migration.sql");
    assert!(edge(
        &g,
        find(&g, "migration", "ALTER child@1"),
        parent,
        "references"
    ));
    put(d.path(), "duplicate.sql", "CREATE TABLE child (id INT);");
    extract(&Options::new(d.path())).unwrap();
    let ambiguous = graph(d.path());
    assert!(!edge(&ambiguous, child, parent, "references"));
}

#[test]
fn php_exit_expression_and_keyword_property_interpolation_keep_calls_and_lines() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "a.php",
        r#"<?php
function target() { return 1; }
function bare() { exit -target(); }
function numeric() { exit -1; }
function parens() { exit(target()); }
function empty_exit() { exit(); }
function interpolate($item, $key) { $s = "\0$item->class\0$key"; target(); }
"#,
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    for caller in ["bare", "parens", "interpolate"] {
        assert!(edge(
            &g,
            find(&g, "function", caller),
            find(&g, "function", "target"),
            "calls"
        ));
    }
    assert_eq!(find(&g, "function", "interpolate")["source_location"], "L7");
    put(
        d.path(),
        "invalid.php",
        "<?php function broken( { exit -1; }",
    );
    assert!(extract(&Options::new(d.path())).is_err());
}

#[test]
fn mysql_dump_keys_and_statement_leading_comments_preserve_schema() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "schema.sql",
        "--CREATE TABLE fake (id INT);\nCREATE TABLE real_table (id INT);\n/*!40000 ALTER TABLE real_table DISABLE KEYS */;\nINSERT INTO real_table VALUES (1);\n/*!40000 ALTER TABLE real_table ENABLE KEYS */;\n--ALTER TABLE fake ADD COLUMN z INT;\nCREATE TABLE child (id INT REFERENCES real_table(id));",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    assert!(
        !g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "fake")
    );
    for label in [
        "ALTER real_table DISABLE KEYS@3",
        "ALTER real_table ENABLE KEYS@5",
    ] {
        assert!(edge(
            &g,
            find(&g, "migration", label),
            find(&g, "table", "real_table"),
            "alters"
        ));
    }
    assert!(edge(
        &g,
        find(&g, "table", "child"),
        find(&g, "table", "real_table"),
        "references"
    ));
    put(
        d.path(),
        "invalid.sql",
        "ALTER TABLE real_table DISABLE KEYS nonsense;",
    );
    assert!(extract(&Options::new(d.path())).is_err());
}

#[test]
fn mixed_sql_auto_dialect_keeps_postgres_bodies_intact() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "mysql/tables.sql",
        "CREATE TABLE `MyTable` (`id` INT);",
    );
    put(
        d.path(),
        "pgsql/functions.sql",
        "CREATE LANGUAGE plpgsql;\nCREATE OR REPLACE FUNCTION answer() RETURNS integer AS $$ BEGIN RETURN 42; END; $$ LANGUAGE plpgsql;\nCREATE TABLE \"PgTable\" (id INTEGER);",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    find(&g, "language", "plpgsql");
    find(&g, "table", "\"PgTable\"");
    find(&g, "table", "`MyTable`");
    assert_eq!(
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] == "sql_statement")
            .count(),
        1
    );
    put(
        d.path(),
        "pgsql/broken.sql",
        "CREATE LANGUAGE plpgsql unexpected;",
    );
    assert!(extract(&Options::new(d.path())).is_err());
}

fn located<'a>(g: &'a Value, file: &str, label: &str) -> &'a Value {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source_file"] == file && n["label"] == label)
        .unwrap()
}

#[test]
fn sql_schema_and_two_dump_versions_resolve_to_their_own_tables() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "install/mysql/tables/users.sql",
        "CREATE TABLE users(id INT);",
    );
    put(
        d.path(),
        "install/mysql/tables/posts.sql",
        "CREATE TABLE posts(uid INT);",
    );
    put(
        d.path(),
        "install/mysql/migration/fk.sql",
        "ALTER TABLE posts ADD FOREIGN KEY(uid) REFERENCES users(id);",
    );
    for file in ["backup/old.sql", "backup/new.sql"] {
        put(
            d.path(),
            file,
            "-- MySQL dump 10\nCREATE TABLE users(id INT); CREATE TABLE posts(uid INT REFERENCES users(id)); ALTER TABLE posts ADD FOREIGN KEY(uid) REFERENCES users(id); CREATE TABLE incomplete(uid INT REFERENCES absent(id));",
        );
    }
    put(
        d.path(),
        "backup/other.sql",
        "-- MariaDB dump 10\nCREATE TABLE absent(id INT);",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    assert!(edge(
        &g,
        located(&g, "install/mysql/tables/posts.sql", "posts"),
        located(&g, "install/mysql/tables/users.sql", "users"),
        "references"
    ));
    for file in ["backup/old.sql", "backup/new.sql"] {
        assert!(edge(
            &g,
            located(&g, file, "posts"),
            located(&g, file, "users"),
            "references"
        ));
        let owner = located(&g, file, "incomplete");
        assert!(
            !g["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["source"] == owner["id"] && e["relation"] == "references")
        );
    }
    for e in g["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "references")
    {
        let source = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == e["source"])
            .unwrap();
        let target = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == e["target"])
            .unwrap();
        if source["source_file"]
            .as_str()
            .unwrap()
            .starts_with("backup/")
        {
            assert_eq!(source["source_file"], target["source_file"]);
        }
    }
}

#[test]
fn sql_scope_overrides_group_split_schema_but_never_mix_dialects() {
    let d = TempDir::new().unwrap();
    put(d.path(), "defs/mysql/a.sql", "CREATE TABLE users(id INT);");
    put(
        d.path(),
        "migrations/mysql/b.sql",
        "ALTER TABLE users ADD COLUMN title TEXT;",
    );
    put(d.path(), "postgres/c.sql", "CREATE TABLE users(id INT);");
    let mut o = Options::new(d.path());
    extract(&o).unwrap();
    let g = graph(d.path());
    assert!(!edge(
        &g,
        located(&g, "migrations/mysql/b.sql", "ALTER users@1"),
        located(&g, "defs/mysql/a.sql", "users"),
        "alters"
    ));
    o.sql_scope = vec!["**/*.sql=app".into()];
    extract(&o).unwrap();
    let g = graph(d.path());
    assert!(edge(
        &g,
        located(&g, "migrations/mysql/b.sql", "ALTER users@1"),
        located(&g, "defs/mysql/a.sql", "users"),
        "alters"
    ));
    assert!(!edge(
        &g,
        located(&g, "migrations/mysql/b.sql", "ALTER users@1"),
        located(&g, "postgres/c.sql", "users"),
        "alters"
    ));
    o.sql_scope.push("defs/**=other".into());
    assert!(extract(&o).is_err());
    assert_eq!(g, graph(d.path()));
}

#[test]
fn sql_qualified_schema_and_use_do_not_collapse_database_names() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "a.sql",
        "USE one;\nCREATE TABLE users(id INT);\nCREATE TABLE posts(uid INT REFERENCES users(id));\nUSE two;\nCREATE TABLE users(id INT);\nCREATE TABLE posts(uid INT REFERENCES users(id));\nCREATE TABLE cross_db(uid INT REFERENCES one.users(id));",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    let posts: Vec<_> = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "table" && n["label"] == "posts")
        .collect();
    let users: Vec<_> = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "table" && n["label"] == "users")
        .collect();
    assert_eq!(posts.len(), 2);
    assert_eq!(users.len(), 2);
    let targets: std::collections::HashSet<_> = g["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "references" && posts.iter().any(|p| p["id"] == e["source"]))
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets.len(), 2);
    let one = users.iter().find(|n| n["source_location"] == "L2").unwrap();
    let two = users.iter().find(|n| n["source_location"] == "L5").unwrap();
    assert!(edge(
        &g,
        posts.iter().find(|n| n["source_location"] == "L3").unwrap(),
        one,
        "references"
    ));
    assert!(edge(
        &g,
        posts.iter().find(|n| n["source_location"] == "L6").unwrap(),
        two,
        "references"
    ));
    assert!(edge(
        &g,
        located(&g, "a.sql", "cross_db"),
        one,
        "references"
    ));
}

#[test]
fn sql_duplicate_declarations_stay_ambiguous_and_keys_isolate_headerless_dump() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "a.sql",
        "CREATE TABLE users(id INT); CREATE TABLE users(id INT); CREATE TABLE posts(uid INT REFERENCES users(id));",
    );
    put(d.path(), "other.sql", "CREATE TABLE users(id INT);");
    put(
        d.path(),
        "bare.sql",
        "CREATE TABLE child(uid INT REFERENCES users(id)); ALTER TABLE child DISABLE KEYS;",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relation"] == "references")
    );
    assert_eq!(
        located(&g, "bare.sql", "bare.sql")["sql_scope"],
        "file:bare.sql"
    );
    assert!(edge(
        &g,
        located(&g, "bare.sql", "ALTER child DISABLE KEYS@1"),
        located(&g, "bare.sql", "child"),
        "alters"
    ));
}

#[test]
fn sql_schema_priority_preserves_local_history_and_rejects_peer_ambiguity() {
    let d = TempDir::new().unwrap();
    put(
        d.path(),
        "mysql/tables/a.sql",
        "CREATE TABLE users(id INT);",
    );
    put(
        d.path(),
        "mysql/migration/v1.sql",
        "CREATE TABLE users(id INT); ALTER TABLE users ADD COLUMN a INT; CREATE TABLE legacy(id INT);",
    );
    put(
        d.path(),
        "mysql/migration/v2.sql",
        "CREATE TABLE users(id INT);",
    );
    put(
        d.path(),
        "mysql/migration/alter.sql",
        "ALTER TABLE users ADD COLUMN b INT; ALTER TABLE legacy ADD COLUMN c INT;",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    assert!(edge(
        &g,
        located(&g, "mysql/migration/v1.sql", "ALTER users@1"),
        located(&g, "mysql/migration/v1.sql", "users"),
        "alters"
    ));
    assert!(edge(
        &g,
        located(&g, "mysql/migration/alter.sql", "ALTER users@1"),
        located(&g, "mysql/tables/a.sql", "users"),
        "alters"
    ));
    assert!(edge(
        &g,
        located(&g, "mysql/migration/alter.sql", "ALTER legacy@1"),
        located(&g, "mysql/migration/v1.sql", "legacy"),
        "alters"
    ));
    put(
        d.path(),
        "mysql/tables/duplicate.sql",
        "CREATE TABLE users(id INT);",
    );
    put(
        d.path(),
        "mysql/migration/v3.sql",
        "CREATE TABLE legacy(id INT);",
    );
    extract(&Options::new(d.path())).unwrap();
    let g = graph(d.path());
    for label in ["ALTER users@1", "ALTER legacy@1"] {
        let caller = located(&g, "mysql/migration/alter.sql", label);
        assert!(
            !g["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["source"] == caller["id"] && e["relation"] == "alters")
        );
    }
}

#[test]
fn sql_explicit_file_isolation_and_dump_text_in_data_are_not_confused() {
    let d = TempDir::new().unwrap();
    put(d.path(), "defs.sql", "CREATE TABLE users(id INT);");
    put(
        d.path(),
        "custom.sql",
        "CREATE TABLE posts(uid INT REFERENCES users(id)); INSERT INTO posts VALUES ('\n-- MySQL dump fake header in data\n');",
    );
    let mut o = Options::new(d.path());
    extract(&o).unwrap();
    let g = graph(d.path());
    assert!(edge(
        &g,
        located(&g, "custom.sql", "posts"),
        located(&g, "defs.sql", "users"),
        "references"
    ));
    o.sql_scope = vec!["custom.sql=@file".into()];
    extract(&o).unwrap();
    let g = graph(d.path());
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relation"] == "references")
    );
}

#[test]
fn parallel_shards_preserve_full_graph_and_stats_across_boundaries() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // More than two four-worker windows, with cross-file references and repeated
    // FK evidence. Include SQL contexts and HTML rather than only PHP nodes.
    for i in 0..270 {
        put(
            root,
            &format!("f{i:03}.php"),
            &format!("<?php function f{i}() {{ f{}(); }} f{i}();", (i + 1) % 270),
        );
    }
    put(
        root,
        "mysql/tables/a.sql",
        "CREATE TABLE a(id INT PRIMARY KEY); CREATE TABLE b(aid INT);",
    );
    for i in 0..40 {
        put(
            root,
            &format!("mysql/tables/key{i:02}.sql"),
            "ALTER TABLE b ADD FOREIGN KEY (aid) REFERENCES a(id);",
        );
    }
    put(
        root,
        "dump.sql",
        "-- MySQL dump\nCREATE TABLE a(id INT); CREATE TABLE b(aid INT REFERENCES a(id));",
    );
    put(root, "index.html", "<a href='f001.php'>go</a>");
    let mut options = Options::new(root);
    options.exclude_file.push("f269.php".into());
    extract(&options).unwrap();
    let expected = fs::read(root.join("graphify-out/graph.json")).unwrap();
    for workers in [2, 4, 8] {
        options.workers = workers;
        extract(&options).unwrap();
        assert_eq!(
            expected,
            fs::read(root.join("graphify-out/graph.json")).unwrap(),
            "workers={workers}"
        );
    }
}

#[test]
fn parallel_failure_and_zero_workers_preserve_published_graph() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for i in 0..100 {
        put(root, &format!("{i}.php"), "<?php function ok() {} ");
    }
    let mut options = Options::new(root);
    options.workers = 4;
    extract(&options).unwrap();
    let expected = fs::read(root.join("graphify-out/graph.json")).unwrap();
    put(root, "bad.php", "<?php function {");
    assert!(extract(&options).is_err());
    assert_eq!(
        expected,
        fs::read(root.join("graphify-out/graph.json")).unwrap()
    );
    assert_eq!(
        fs::read_dir(root.join("graphify-out")).unwrap().count(),
        1,
        "temporary shards must be removed on failure"
    );
    fs::remove_file(root.join("bad.php")).unwrap();
    options.workers = 0;
    assert!(extract(&options).is_err());
    assert_eq!(
        expected,
        fs::read(root.join("graphify-out/graph.json")).unwrap()
    );
    options.workers = 4;
    extract(&options).unwrap();
    assert_eq!(
        expected,
        fs::read(root.join("graphify-out/graph.json")).unwrap()
    );
}

#[test]
fn receiver_class_lookup_keeps_file_local_bindings_and_global_ambiguity() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for (file, function) in [("a.php", "fromA"), ("b.php", "fromB")] {
        put(
            root,
            file,
            &format!(
                "<?php class Shared {{ function ping() {{}} }} function {function}(Shared $x) {{ $x->ping(); $x->ping(); }}"
            ),
        );
    }
    put(
        root,
        "c.php",
        "<?php function fromC(Shared $x) { $x->ping(); }",
    );
    extract(&Options::new(root)).unwrap();
    let g = graph(root);
    for (file, function) in [("a.php", "fromA"), ("b.php", "fromB")] {
        let source = find(&g, "function", function);
        let targets: Vec<_> = g["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source"] == source["id"] && e["relation"] == "calls")
            .map(|e| {
                g["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["id"] == e["target"])
                    .unwrap()
            })
            .collect();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["source_file"], file);
        assert_eq!(targets[0]["label"], "Shared::ping");
    }
    let ambiguous = find(&g, "function", "fromC");
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == ambiguous["id"] && e["relation"] == "calls")
    );
}
