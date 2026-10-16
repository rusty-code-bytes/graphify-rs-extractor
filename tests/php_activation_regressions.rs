use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, source).unwrap();
}

fn graph(options: &Options) -> Value {
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
        .unwrap_or_else(|| panic!("missing {file}: {label}"))
}

fn calls<'a>(graph: &'a Value, source: &Value, name: &str) -> Vec<&'a Value> {
    graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| {
            edge["source"] == source["id"]
                && edge["relation"] == "calls"
                && graph["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|target| target["id"] == edge["target"] && target["label"] == name)
        })
        .collect()
}

fn unknown_inventory(root: &Path) {
    put(
        root,
        "composer.json",
        r#"{"autoload":{"files":["loader.php"]}}"#,
    );
    put(root, "loader.php", "<?php require $runtime;\n");
}

fn assert_call(graph: &Value, caller: &str, owner: &str, selected: &str, name: &str, line: usize) {
    let source = node(graph, caller, owner);
    let target = node(graph, selected, name);
    let edges = calls(graph, source, name);
    assert_eq!(edges.len(), 1, "{caller} {owner} -> {selected} {name}");
    assert_eq!(edges[0]["target"], target["id"]);
    assert_eq!(edges[0]["source_file"], caller);
    assert_eq!(edges[0]["source_location"], format!("L{line}"));
    assert_eq!(target["source_location"], "L2");
}

#[test]
fn exact_activation_selects_required_duplicate_despite_unknown_inventory() {
    for workers in [1, 4] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(dir.path(), "selected.php", "<?php\nfunction helper() {}\n");
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "caller.php",
            "<?php\nrequire __DIR__ . '/selected.php'; helper();\n",
        );
        let mut options = Options::new(dir.path());
        options.workers = workers;
        let g = graph(&options);
        assert_call(&g, "caller.php", "caller.php", "selected.php", "helper", 2);
        let include = g["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| {
                edge["source"] == node(&g, "caller.php", "caller.php")["id"]
                    && edge["target"] == node(&g, "selected.php", "selected.php")["id"]
                    && edge["relation"] == "includes"
            })
            .unwrap();
        assert_eq!(include["source_location"], "L2");
    }
}

#[test]
fn exact_namespace_and_import_proofs_do_not_enable_unknown_namespace_fallback() {
    for (declaration, caller, name) in [
        (
            "function helper() {}",
            "namespace App; use function \\helper as selected;\nrequire __DIR__ . '/selected.php'; selected();",
            "helper",
        ),
        (
            "function helper() {}",
            "namespace App;\nrequire __DIR__ . '/selected.php'; \\helper();",
            "helper",
        ),
        (
            "namespace App; function helper() {}",
            "namespace App;\nrequire __DIR__ . '/selected.php'; helper();",
            "App\\helper",
        ),
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(
            dir.path(),
            "selected.php",
            &format!("<?php\n{declaration}\n"),
        );
        put(dir.path(), "decoy.php", &format!("<?php\n{declaration}\n"));
        put(dir.path(), "caller.php", &format!("<?php\n{caller}\n"));
        let g = graph(&Options::new(dir.path()));
        assert_call(&g, "caller.php", "caller.php", "selected.php", name, 3);
    }
    for composer in [false, true] {
        let dir = TempDir::new().unwrap();
        if composer {
            unknown_inventory(dir.path());
        }
        put(dir.path(), "selected.php", "<?php\nfunction helper() {}\n");
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "init.php",
            "<?php eval('namespace App; function helper() {}');\n",
        );
        put(
            dir.path(),
            "caller.php",
            "<?php namespace App;\nrequire __DIR__ . '/selected.php';\nrequire __DIR__ . '/init.php';\nhelper();\n",
        );
        let g = graph(&Options::new(dir.path()));
        assert!(calls(&g, node(&g, "caller.php", "caller.php"), "helper").is_empty());
    }
}

#[test]
fn proof_is_same_execution_scope_and_does_not_cross_deferred_callbacks() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "bootstrap.php",
        "<?php eval('function helper() {}'); callback(); exit;\n",
    );
    put(
        dir.path(),
        "caller.php",
        "<?php\nrequire __DIR__ . '/bootstrap.php';\nrequire __DIR__ . '/native.php';\nfunction callback() { helper(); }\nif ($runtime) { function helper() {} }\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert!(calls(&g, node(&g, "caller.php", "callback"), "helper").is_empty());

    unknown_inventory(dir.path());
    put(
        dir.path(),
        "caller.php",
        "<?php\nfunction callback() { require __DIR__ . '/native.php'; helper(); }\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert_call(&g, "caller.php", "callback", "native.php", "helper", 2);
}

#[test]
fn unsupported_control_flow_and_expression_order_stay_unproven() {
    for source in [
        "if ($runtime) { require __DIR__ . '/native.php'; } helper();",
        "try { require __DIR__ . '/native.php'; } catch (Throwable $e) {} helper();",
        "consume(helper(), require __DIR__ . '/native.php');",
        "consume(require __DIR__ . '/native.php', helper());",
        "false && require __DIR__ . '/native.php'; helper();",
        "helper(); require __DIR__ . '/native.php';",
        "goto after; require __DIR__ . '/native.php'; after: helper();",
        "function invoke() { goto after; require __DIR__ . '/native.php'; after: helper(); }",
        "return; require __DIR__ . '/native.php'; helper();",
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(dir.path(), "caller.php", &format!("<?php\n{source}\n"));
        let g = graph(&Options::new(dir.path()));
        let owner = if source.starts_with("function") {
            "invoke"
        } else {
            "caller.php"
        };
        assert!(
            calls(&g, node(&g, "caller.php", owner), "helper").is_empty(),
            "{source}"
        );
    }
}

#[test]
fn later_direct_require_can_prove_normal_continuation_after_an_early_return_branch() {
    let dir = TempDir::new().unwrap();
    unknown_inventory(dir.path());
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "caller.php",
        "<?php\nif ($stop) { return; }\nopaque();\nrequire __DIR__ . '/native.php';\nopaque();\nhelper();\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert_call(&g, "caller.php", "caller.php", "native.php", "helper", 6);
}

#[test]
fn missing_excluded_and_duplicate_required_targets_cannot_select_decoys() {
    for state in [
        "missing",
        "excluded",
        "duplicate",
        "conditional",
        "two-required",
    ] {
        let dir = TempDir::new().unwrap();
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "caller.php",
            "<?php\nrequire __DIR__ . '/native.php'; helper();\n",
        );
        if state != "missing" {
            let source = match state {
                "duplicate" => "<?php\nfunction helper() {}\nfunction helper() {}\n",
                "conditional" => {
                    "<?php\nthrow new Exception; if ($runtime) { function helper() {} }\n"
                }
                _ => "<?php\nfunction helper() {}\n",
            };
            put(dir.path(), "native.php", source);
        }
        if state == "two-required" {
            put(
                dir.path(),
                "caller.php",
                "<?php\nrequire __DIR__ . '/native.php'; require __DIR__ . '/decoy.php'; helper();\n",
            );
        }
        let mut options = Options::new(dir.path());
        if state == "excluded" {
            options.exclude_file.push("native.php".into());
        }
        let g = graph(&options);
        assert!(
            calls(&g, node(&g, "caller.php", "caller.php"), "helper").is_empty(),
            "{state}"
        );
    }
}

#[test]
fn unconditional_definitions_after_return_activate_but_caught_conditional_failures_do_not() {
    let dir = TempDir::new().unwrap();
    unknown_inventory(dir.path());
    put(
        dir.path(),
        "native.php",
        "<?php\nreturn;\nfunction helper() {}\n",
    );
    put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "caller.php",
        "<?php\nrequire __DIR__.'/native.php'; helper();\n",
    );
    let g = graph(&Options::new(dir.path()));
    let target = node(&g, "native.php", "helper");
    assert_eq!(target["source_location"], "L3");
    let selected = calls(&g, node(&g, "caller.php", "caller.php"), "helper");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["target"], target["id"]);
    assert_eq!(selected[0]["source_location"], "L2");

    put(
        dir.path(),
        "native.php",
        "<?php\nthrow new Exception;\nif ($runtime) { function helper() {} }\n",
    );
    put(
        dir.path(),
        "caller.php",
        "<?php\ntry { require __DIR__.'/native.php'; } catch (Throwable $e) {}\nif (!function_exists('helper')) { function helper() {} }\nhelper();\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert!(calls(&g, node(&g, "caller.php", "caller.php"), "helper").is_empty());
}

#[test]
fn unsupported_hack_dependency_remains_inventoried_and_cannot_select_a_decoy() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "native.php",
        "<?hh\nfunction helper(): void {}\n",
    );
    put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "caller.php",
        "<?php\nrequire __DIR__.'/native.php'; helper();\n",
    );
    let options = Options::new(dir.path());
    let stats = extract(&options).unwrap();
    assert_eq!(stats.unsupported_hack_files, 1);
    let g: Value =
        serde_json::from_slice(&fs::read(dir.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    assert_eq!(node(&g, "native.php", "native.php")["language"], "hack");
    assert!(calls(&g, node(&g, "caller.php", "caller.php"), "helper").is_empty());
}

#[test]
fn same_file_unconditional_exact_functions_survive_unknown_required_inventory() {
    for dependency in ["missing", "excluded", "hack"] {
        let dir = TempDir::new().unwrap();
        put(
            dir.path(),
            "caller.php",
            "<?php\nfunction local_helper() {}\nrequire __DIR__.'/hidden.php';\nlocal_helper();\n",
        );
        if dependency != "missing" {
            put(
                dir.path(),
                "hidden.php",
                if dependency == "hack" {
                    "<?hh function unrelated(): void {}"
                } else {
                    "<?php function unrelated() {}"
                },
            );
        }
        let mut options = Options::new(dir.path());
        if dependency == "excluded" {
            options.exclude_file.push("hidden.php".into());
        }
        let g = graph(&options);
        assert_call(
            &g,
            "caller.php",
            "caller.php",
            "caller.php",
            "local_helper",
            4,
        );
    }
}

#[test]
fn bounded_paths_support_locals_dirname_and_fresh_builtin_object_properties() {
    for prefix in [
        "$root = \\dirname(__DIR__); $library = $root . '/lib'; require $library . '/native.php';",
        "$CFG = new \\stdClass(); $CFG->libdir = \\dirname(__DIR__) . '/lib'; require $CFG->libdir . '/native.php';",
        "$root = dirname(__DIR__); require $root . '/lib/native.php';",
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(
            dir.path(),
            "lib/native.php",
            "<?php\nfunction helper() {}\n",
        );
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "sub/caller.php",
            &format!("<?php\nfunction invoke() {{ {prefix} helper(); }}\n"),
        );
        let g = graph(&Options::new(dir.path()));
        assert_call(
            &g,
            "sub/caller.php",
            "invoke",
            "lib/native.php",
            "helper",
            2,
        );
    }
}

#[test]
fn opaque_effects_aliases_and_magic_objects_invalidate_computed_paths() {
    for effect in [
        "opaque();",
        "eval($runtime);",
        "include __DIR__ . '/effects.php';",
        "$alias = $CFG; $alias->libdir = __DIR__ . '/decoy';",
        "$alias =& $CFG;",
        "global $CFG;",
        "unset($CFG->libdir);",
        "$CFG->$runtime = 'value';",
        "class Child extends Missing {}",
        "$other = new \\stdClass(opaque());",
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(dir.path(), "effects.php", "<?php\n");
        put(
            dir.path(),
            "caller.php",
            &format!(
                "<?php\nfunction invoke() {{ $CFG = new \\stdClass(); $CFG->libdir = __DIR__; {effect} require $CFG->libdir . '/native.php'; helper(); }}\n"
            ),
        );
        let g = graph(&Options::new(dir.path()));
        assert!(
            calls(&g, node(&g, "caller.php", "invoke"), "helper").is_empty(),
            "{effect}"
        );
    }
    for declaration in [
        "namespace App; class stdClass {}",
        "class Custom { function __get($n) {} }",
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        let constructor = if declaration.starts_with("namespace") {
            "stdClass"
        } else {
            "Custom"
        };
        put(
            dir.path(),
            "caller.php",
            &format!(
                "<?php\n{declaration}\n$CFG = new {constructor}(); $CFG->libdir=__DIR__; require $CFG->libdir . '/native.php'; \\helper();\n"
            ),
        );
        let g = graph(&Options::new(dir.path()));
        assert!(calls(&g, node(&g, "caller.php", "caller.php"), "helper").is_empty());
    }
}

#[test]
fn namespace_shadowed_dirname_and_imports_cannot_fold_as_builtins() {
    for prefix in [
        "namespace App; function dirname($value) { return __DIR__ . '/alternate'; }",
        "namespace App; use function Other\\dirname;",
        "use function Other\\dirname;",
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "alternate/native.php",
            "<?php\nfunction helper() {}\n",
        );
        put(
            dir.path(),
            "sub/caller.php",
            &format!(
                "<?php\n{prefix}\nfunction invoke() {{ $root = dirname(__DIR__); require $root . '/native.php'; \\helper(); }}\n"
            ),
        );
        let g = graph(&Options::new(dir.path()));
        let owner = if prefix.starts_with("namespace") {
            "App\\invoke"
        } else {
            "invoke"
        };
        assert!(
            calls(&g, node(&g, "sub/caller.php", owner), "helper").is_empty(),
            "{prefix}"
        );
    }
}

#[test]
fn implicit_destructors_ticks_and_unknown_callable_inputs_prevent_path_proof() {
    for source in [
        "class Mutator { function __destruct() { global $CFG; $CFG->libdir=__DIR__.'/alternate'; } }\n$x=new Mutator; $CFG=new \\stdClass; $CFG->libdir=__DIR__; $x='safe'; require $CFG->libdir.'/native.php'; helper();",
        "declare(ticks=1); function tick() { global $CFG; if ($CFG) { $CFG->libdir=__DIR__.'/alternate'; } }\nregister_tick_function('tick'); $CFG=new \\stdClass; $CFG->libdir=__DIR__; require $CFG->libdir.'/native.php'; helper();",
        "function invoke($CFG) { $root=__DIR__; $CFG='safe'; require $root.'/native.php'; helper(); }",
        "function invoke($root) { $root=__DIR__; require $root.'/native.php'; helper(); }",
        "function invoke() { $_SESSION=__DIR__; require $_SESSION.'/native.php'; helper(); }",
        "function invoke() { $x=new Mutator; $CFG=new \\stdClass; $CFG->libdir=__DIR__; $x='safe'; require $CFG->libdir.'/native.php'; helper(); }",
        "function invoke() { eval($runtime); $root=__DIR__; require $root.'/native.php'; helper(); }",
        "$root=__DIR__; $closure = function() use (&$root) { require $root.'/native.php'; helper(); };",
    ] {
        let dir = TempDir::new().unwrap();
        unknown_inventory(dir.path());
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "alternate/native.php",
            "<?php\nfunction helper() {}\n",
        );
        put(dir.path(), "caller.php", &format!("<?php\n{source}\n"));
        let g = graph(&Options::new(dir.path()));
        let targets: Vec<_> = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["label"] == "helper")
            .map(|node| node["id"].clone())
            .collect();
        assert!(
            !g["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["source_file"] == "caller.php"
                    && edge["relation"] == "calls"
                    && targets.contains(&edge["target"])),
            "{source}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_parent_traversal_cannot_select_a_lexical_decoy() {
    let dir = TempDir::new().unwrap();
    unknown_inventory(dir.path());
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "alternate/native.php",
        "<?php\nfunction helper() {}\n",
    );
    fs::create_dir_all(dir.path().join("alternate/deep")).unwrap();
    std::os::unix::fs::symlink("alternate/deep", dir.path().join("link")).unwrap();
    put(
        dir.path(),
        "caller.php",
        "<?php\nrequire __DIR__ . '/link/../native.php'; helper();\n",
    );
    let g = graph(&Options::new(dir.path()));
    let source = node(&g, "caller.php", "caller.php");
    assert!(calls(&g, source, "helper").is_empty());
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == source["id"] && edge["relation"] == "includes")
    );
}

#[test]
fn missing_path_separator_cannot_normalize_to_an_existing_ancestor_file() {
    let dir = TempDir::new().unwrap();
    unknown_inventory(dir.path());
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "a/b/c/valid.php",
        "<?php\nrequire __DIR__ . '/../../../native.php';\nhelper();\n",
    );
    put(
        dir.path(),
        "a/b/c/invalid.php",
        "<?php\nrequire __DIR__ . '../../../native.php';\nhelper();\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert_call(
        &g,
        "a/b/c/valid.php",
        "a/b/c/valid.php",
        "native.php",
        "helper",
        3,
    );
    let invalid = node(&g, "a/b/c/invalid.php", "a/b/c/invalid.php");
    assert!(calls(&g, invalid, "helper").is_empty());
    assert!(
        !g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| { edge["source"] == invalid["id"] && edge["relation"] == "includes" })
    );
    let valid = node(&g, "a/b/c/valid.php", "a/b/c/valid.php");
    let target = node(&g, "native.php", "native.php");
    assert!(g["links"].as_array().unwrap().iter().any(|edge| {
        edge["source"] == valid["id"]
            && edge["target"] == target["id"]
            && edge["relation"] == "includes"
            && edge["source_file"] == "a/b/c/valid.php"
            && edge["source_location"] == "L2"
    }));
}

#[test]
fn malformed_required_source_fails_without_publishing_a_partial_graph() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "caller.php",
        "<?php\nrequire __DIR__ . '/native.php'; helper();\n",
    );
    put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
    put(dir.path(), "native.php", "<?php function helper( {\n");
    let error = extract(&Options::new(dir.path())).unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("native.php"), "{diagnostic}");
    assert!(
        diagnostic.contains("refusing partial extraction"),
        "{diagnostic}"
    );
    assert!(!dir.path().join("graphify-out/graph.json").exists());
}

#[test]
fn cached_require_facts_rebuild_resolution_after_deletion_exclusion_and_restore() {
    let dir = TempDir::new().unwrap();
    unknown_inventory(dir.path());
    put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "caller.php",
        "<?php\nrequire __DIR__ . '/native.php'; helper();\n",
    );
    let cache = TempDir::new().unwrap();
    let mut options = Options::new(dir.path());
    options.cache_dir = Some(cache.path().to_path_buf());
    for (state, workers) in [
        ("present", 1),
        ("warm", 4),
        ("excluded", 1),
        ("missing", 4),
        ("restored", 4),
    ] {
        options.workers = workers;
        options.exclude_file.clear();
        if state == "present" || state == "restored" {
            put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        }
        if state == "excluded" {
            options.exclude_file.push("native.php".into());
        }
        if state == "missing" {
            fs::remove_file(dir.path().join("native.php")).unwrap();
        }
        let g = graph(&options);
        if state == "excluded" || state == "missing" {
            assert!(
                calls(&g, node(&g, "caller.php", "caller.php"), "helper").is_empty(),
                "{state}"
            );
        } else {
            assert_call(&g, "caller.php", "caller.php", "native.php", "helper", 2);
        }
    }
}

#[test]
fn computed_path_expansion_is_bounded_and_fails_closed() {
    let dir = TempDir::new().unwrap();
    unknown_inventory(dir.path());
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    let mut source = String::from("<?php\nfunction invoke() { $path=__DIR__ . '/x';\n");
    for _ in 0..64 {
        source.push_str("$path=$path . $path;\n");
    }
    source.push_str("require $path . '/native.php'; helper(); }\n");
    put(dir.path(), "caller.php", &source);
    let g = graph(&Options::new(dir.path()));
    assert!(calls(&g, node(&g, "caller.php", "invoke"), "helper").is_empty());
}

#[test]
fn dormant_effects_do_not_taint_exact_proof_but_still_block_unproven_calls() {
    for effect in ["eval($runtime);", "require $runtime;"] {
        let dir = TempDir::new().unwrap();
        put(
            dir.path(),
            "composer.json",
            r#"{"autoload":{"files":["loader.php"]}}"#,
        );
        put(
            dir.path(),
            "loader.php",
            &format!("<?php function dormant() {{ {effect} }}\n"),
        );
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "caller.php",
            "<?php\nfunction invoke() { helper(); }\nrequire __DIR__ . '/native.php'; helper();\n",
        );
        let g = graph(&Options::new(dir.path()));
        assert!(calls(&g, node(&g, "caller.php", "invoke"), "helper").is_empty());
        assert_call(&g, "caller.php", "caller.php", "native.php", "helper", 3);
        put(
            dir.path(),
            "loader.php",
            &format!("<?php function dormant() {{ {effect} }} dormant();\n"),
        );
        let g = graph(&Options::new(dir.path()));
        assert!(calls(&g, node(&g, "caller.php", "invoke"), "helper").is_empty());
        assert_call(&g, "caller.php", "caller.php", "native.php", "helper", 3);
    }
}
