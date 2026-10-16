use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, source: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, source).unwrap();
}

fn graph(options: &Options) -> Value {
    extract(options).unwrap();
    serde_json::from_slice(&fs::read(options.root.join("graphify-out/graph.json")).unwrap())
        .unwrap()
}

fn node<'a>(graph: &'a Value, file: &str, label: &str, line: usize) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| {
            node["source_file"] == file
                && node["label"] == label
                && node["source_location"] == format!("L{line}")
        })
        .unwrap_or_else(|| panic!("missing {file}:{line} {label}"))
}

fn edges<'a>(graph: &'a Value, source: &Value, relation: &str) -> Vec<&'a Value> {
    graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["source"] == source["id"] && edge["relation"] == relation)
        .collect()
}

fn linked_at(graph: &Value, source: &Value, target: &Value, relation: &str, line: usize) -> bool {
    edges(graph, source, relation)
        .iter()
        .any(|edge| edge["target"] == target["id"] && edge["source_location"] == format!("L{line}"))
}

#[test]
fn guarded_polyfill_cannot_override_a_required_unconditional_declaration() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "polyfill.php",
        "<?php\nrequire_once __DIR__ . '/native.php';\nif (!function_exists('helper')) {\n function helper() {}\n}\nfunction caller() {\n require_once __DIR__ . '/native.php'; helper();\n}\ncaller();\n",
    );
    let graph = graph(&Options::new(dir.path()));
    let caller = node(&graph, "polyfill.php", "caller", 6);
    let native = node(&graph, "native.php", "helper", 2);
    let conditional = node(&graph, "polyfill.php", "helper", 4);
    assert!(linked_at(&graph, caller, native, "calls", 7));
    assert!(!linked_at(&graph, caller, conditional, "calls", 7));
    let file = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["type"] == "file" && n["source_file"] == "polyfill.php")
        .unwrap();
    assert!(linked_at(&graph, file, caller, "calls", 9));
}

#[test]
fn conditional_functions_stay_unresolved_without_a_unique_visible_unconditional_target() {
    for variant in [
        "missing",
        "indeterminate",
        "excluded",
        "duplicates",
        "nested",
    ] {
        let dir = TempDir::new().unwrap();
        let source = if variant == "nested" {
            "<?php\nfunction setup() { function helper() {} }\nfunction caller() { helper(); }\ncaller();\n"
        } else {
            "<?php\nif ($runtime) { function helper() {} }\nfunction caller() { helper(); }\ncaller();\n"
        };
        put(dir.path(), "polyfill.php", source);
        if matches!(variant, "indeterminate" | "excluded" | "duplicates") {
            put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        }
        if variant == "duplicates" {
            put(dir.path(), "duplicate.php", "<?php\nfunction helper() {}\n");
        }
        let mut options = Options::new(dir.path());
        if variant == "excluded" {
            options.exclude_file.push("native.php".into());
        }
        let graph = graph(&options);
        let caller = node(&graph, "polyfill.php", "caller", 3);
        node(&graph, "polyfill.php", "helper", 2);
        assert!(edges(&graph, caller, "calls").is_empty(), "{variant}");
    }
}

#[test]
fn a_conditional_or_bypassed_require_cannot_prove_a_polyfill_target() {
    for prefix in [
        "if ($runtime) { require_once __DIR__ . '/native.php'; }",
        "if ($runtime) { return; }\nrequire_once __DIR__ . '/native.php';",
        "unknown();\nrequire_once __DIR__ . '/native.php';",
    ] {
        let dir = TempDir::new().unwrap();
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        let prefix_lines = prefix.lines().count();
        put(
            dir.path(),
            "polyfill.php",
            &format!(
                "<?php\n{prefix}\nif (!function_exists('helper')) {{ function helper() {{}} }}\nfunction caller() {{ helper(); }}\n"
            ),
        );
        let g = graph(&Options::new(dir.path()));
        let caller = node(&g, "polyfill.php", "caller", prefix_lines + 3);
        node(&g, "polyfill.php", "helper", prefix_lines + 2);
        node(&g, "native.php", "helper", 2);
        assert!(edges(&g, caller, "calls").is_empty(), "{prefix}");
    }
}

#[test]
fn file_requires_never_activate_deferred_function_bodies_by_text_order() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "polyfill.php",
        "<?php\nfunction before() { helper(); }\nrequire __DIR__ . '/native.php'; function same_line() { helper(); }\nif (!function_exists('helper')) { function helper() {} }\nfunction after() { helper(); }\n",
    );
    let g = graph(&Options::new(dir.path()));
    for (name, line) in [("before", 2), ("same_line", 3)] {
        assert!(edges(&g, node(&g, "polyfill.php", name, line), "calls").is_empty());
    }
    assert!(edges(&g, node(&g, "polyfill.php", "after", 5), "calls").is_empty());
}

#[test]
fn repeated_definite_and_ordinary_includes_preserve_one_native_edge_and_first_line() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "helper.php", "<?php $value = 1;");
    put(
        dir.path(),
        "main.php",
        "<?php\nrequire __DIR__ . '/helper.php';\ninclude_once __DIR__ . '/helper.php';\n",
    );
    let g = graph(&Options::new(dir.path()));
    let file = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["source_file"] == "main.php" && node["type"] == "file")
        .unwrap();
    let included = edges(&g, file, "includes");
    assert_eq!(included.len(), 1);
    assert_eq!(included[0]["source_location"], "L2");
    assert_eq!(included[0]["context"], "import");
}

#[test]
fn unknown_active_autoload_files_never_rebind_excluded_functions_to_bundled_copies() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "composer.json", "{}");
    put(
        dir.path(),
        "vendor/composer/autoload_files.php",
        "<?php return ['functions' => __DIR__ . '/../library/functions.php'];",
    );
    put(
        dir.path(),
        "vendor/library/functions.php",
        "<?php\nnamespace Lib;\nfunction helper() {}\n",
    );
    put(
        dir.path(),
        "bundled/functions.php",
        "<?php\nnamespace Lib;\nfunction helper() {}\nfunction local_call() { helper(); }\nfunction polyfill_helper() {}\n",
    );
    put(
        dir.path(),
        "caller.php",
        "<?php\nuse function Lib\\helper as imported;\nfunction caller() { imported(); }\n",
    );
    put(
        dir.path(),
        "polyfill.php",
        "<?php namespace Lib;\nrequire __DIR__ . '/bundled/functions.php';\nif ($runtime) { function polyfill_helper() {} }\nfunction polyfill_caller() { polyfill_helper(); }\n",
    );
    put(dir.path(), "nested/composer.json", "{\"type\":\"project\"}");
    put(
        dir.path(),
        "nested/functions.php",
        "<?php\nnamespace Lib;\nfunction helper() {}\nfunction nested_call() { helper(); }\n",
    );
    let mut options = Options::new(dir.path());
    let g = graph(&options);
    assert!(linked_at(
        &g,
        node(&g, "caller.php", "caller", 3),
        node(&g, "vendor/library/functions.php", "Lib\\helper", 3),
        "calls",
        3
    ));
    for missing in [false, true] {
        if missing {
            fs::remove_file(dir.path().join("vendor/library/functions.php")).unwrap();
            options.exclude.clear();
        } else {
            options.exclude.push("vendor/library/**".into());
        }
        let g = graph(&options);
        assert!(edges(&g, node(&g, "caller.php", "caller", 3), "calls").is_empty());
        assert!(
            edges(
                &g,
                node(&g, "polyfill.php", "Lib\\polyfill_caller", 4),
                "calls"
            )
            .is_empty()
        );
        assert!(linked_at(
            &g,
            node(&g, "bundled/functions.php", "Lib\\local_call", 4),
            node(&g, "bundled/functions.php", "Lib\\helper", 3),
            "calls",
            4
        ));
        assert!(linked_at(
            &g,
            node(&g, "nested/functions.php", "Lib\\nested_call", 4),
            node(&g, "nested/functions.php", "Lib\\helper", 3),
            "calls",
            4
        ));
    }
    put(
        dir.path(),
        "vendor/library/functions.php",
        "<?php $constantOnly = 1;",
    );
    let g = graph(&options);
    assert!(linked_at(
        &g,
        node(&g, "caller.php", "caller", 3),
        node(&g, "bundled/functions.php", "Lib\\helper", 3),
        "calls",
        3
    ));
}

#[test]
fn safe_direct_autoload_function_survives_unknown_siblings_while_weaker_fallback_stays_blocked() {
    for unknown in ["dynamic", "hidden"] {
        for generated in [false, true] {
            let dir = TempDir::new().unwrap();
            let cache = TempDir::new().unwrap();
            put(
                dir.path(),
                "composer.json",
                "{\"autoload\":{\"files\":[\"known.php\",\"unknown.php\"]}}",
            );
            if generated {
                put(
                    dir.path(),
                    "vendor/composer/autoload_files.php",
                    "<?php return ['known' => __DIR__ . '/../../known.php', 'unknown' => __DIR__ . '/../../unknown.php'];",
                );
            }
            put(
                dir.path(),
                "known.php",
                "<?php\nnamespace Lib;\nfunction selected() {}\n",
            );
            put(
                dir.path(),
                "unknown.php",
                if unknown == "dynamic" {
                    "<?php\ninclude $runtime;\n"
                } else {
                    "<?php\nfunction unknown_helper() {}\n"
                },
            );
            put(
                dir.path(),
                "decoy.php",
                "<?php\nnamespace Lib;\nfunction selected() {}\nfunction weak_helper() {}\n",
            );
            put(
                dir.path(),
                "caller.php",
                "<?php\nnamespace Lib;\nfunction caller() {\n selected();\n weak_helper();\n}\ncaller();\n",
            );
            let mut options = Options::new(dir.path());
            options.cache_dir = Some(cache.path().into());
            if unknown == "hidden" {
                options.exclude_file.push("unknown.php".into());
            }
            for workers in [1, 3] {
                options.workers = workers;
                let g = graph(&options);
                let caller = node(&g, "caller.php", "Lib\\caller", 3);
                assert!(
                    linked_at(
                        &g,
                        caller,
                        node(&g, "known.php", "Lib\\selected", 3),
                        "calls",
                        4
                    ),
                    "{unknown}, generated={generated}, workers={workers}"
                );
                assert!(!linked_at(
                    &g,
                    caller,
                    node(&g, "decoy.php", "Lib\\selected", 3),
                    "calls",
                    4
                ));
                assert!(!linked_at(
                    &g,
                    caller,
                    node(&g, "decoy.php", "Lib\\weak_helper", 4),
                    "calls",
                    5
                ));
                assert_eq!(edges(&g, caller, "calls").len(), 1);
                let file = g["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|node| node["type"] == "file" && node["source_file"] == "caller.php")
                    .unwrap();
                assert!(linked_at(&g, file, caller, "calls", 7));
                let mut fresh = options.clone();
                fresh.cache_dir = None;
                assert_eq!(g, graph(&fresh));
            }
        }
    }
}

#[test]
fn registered_include_delegation_never_rebinds_an_excluded_native_function_to_a_decoy() {
    for indirect in [false, true] {
        let dir = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        put(
            dir.path(),
            "composer.json",
            "{\"autoload\":{\"files\":[\"registered.php\"]}}",
        );
        put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "registered.php",
            if indirect {
                "<?php\nrequire_once __DIR__ . '/delegate.php';\n"
            } else {
                "<?php\nrequire_once __DIR__ . '/native.php';\n"
            },
        );
        if indirect {
            put(
                dir.path(),
                "delegate.php",
                "<?php\nrequire_once __DIR__ . '/native.php';\n",
            );
        }
        put(
            dir.path(),
            "caller.php",
            "<?php\nfunction caller() { helper(); }\ncaller();\n",
        );
        let mut options = Options::new(dir.path());
        options.cache_dir = Some(cache.path().into());
        for stage in ["visible", "decoy", "excluded", "deleted", "restored"] {
            if stage == "decoy" {
                put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
            } else if stage == "excluded" {
                options.exclude_file.push("native.php".into());
            } else if stage == "deleted" {
                options.exclude_file.clear();
                fs::remove_file(dir.path().join("native.php")).unwrap();
            } else if stage == "restored" {
                put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
                fs::remove_file(dir.path().join("decoy.php")).unwrap();
            }
            for workers in [1, 3] {
                options.workers = workers;
                let g = graph(&options);
                let caller = node(&g, "caller.php", "caller", 2);
                if matches!(stage, "visible" | "restored") {
                    assert!(linked_at(
                        &g,
                        caller,
                        node(&g, "native.php", "helper", 2),
                        "calls",
                        2
                    ));
                } else {
                    assert!(
                        edges(&g, caller, "calls").is_empty(),
                        "{stage}, indirect={indirect}, workers={workers}"
                    );
                    assert!(!linked_at(
                        &g,
                        caller,
                        node(&g, "decoy.php", "helper", 2),
                        "calls",
                        2
                    ));
                }
                let file = g["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|node| node["source_file"] == "caller.php" && node["type"] == "file")
                    .unwrap();
                assert!(linked_at(&g, file, caller, "calls", 3));
                let mut fresh = options.clone();
                fresh.cache_dir = None;
                assert_eq!(g, graph(&fresh));
            }
        }
    }
}

#[test]
fn dynamic_include_and_eval_in_registered_sources_block_unproven_function_targets() {
    for statement in [
        "include $runtime;",
        "require 'runtime.php';",
        "eval($code);",
    ] {
        let dir = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        put(
            dir.path(),
            "composer.json",
            "{\"autoload\":{\"files\":[\"registered.php\"]}}",
        );
        put(
            dir.path(),
            "registered.php",
            &format!("<?php\n{statement}\n"),
        );
        put(dir.path(), "decoy.php", "<?php\nfunction helper() {}\n");
        put(
            dir.path(),
            "caller.php",
            "<?php\nfunction caller() { helper(); }\ncaller();\n",
        );
        let mut options = Options::new(dir.path());
        options.cache_dir = Some(cache.path().into());
        for workers in [1, 3] {
            options.workers = workers;
            let g = graph(&options);
            let caller = node(&g, "caller.php", "caller", 2);
            assert!(
                edges(&g, caller, "calls").is_empty(),
                "{statement}, workers={workers}"
            );
            assert!(!linked_at(
                &g,
                caller,
                node(&g, "decoy.php", "helper", 2),
                "calls",
                2
            ));
            let mut fresh = options.clone();
            fresh.cache_dir = None;
            assert_eq!(g, graph(&fresh));
        }
    }
}

#[test]
fn cyclic_static_autoload_includes_terminate_and_keep_the_visible_native_target() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "composer.json",
        "{\"autoload\":{\"files\":[\"registered.php\"]}}",
    );
    put(
        dir.path(),
        "registered.php",
        "<?php\nrequire_once __DIR__ . '/delegate.php';\n",
    );
    put(
        dir.path(),
        "delegate.php",
        "<?php\nrequire_once __DIR__ . '/registered.php';\nrequire_once __DIR__ . '/native.php';\n",
    );
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "caller.php",
        "<?php\nfunction caller() { helper(); }\n",
    );
    for workers in [1, 3] {
        let mut options = Options::new(dir.path());
        options.workers = workers;
        let g = graph(&options);
        assert!(linked_at(
            &g,
            node(&g, "caller.php", "caller", 2),
            node(&g, "native.php", "helper", 2),
            "calls",
            2
        ));
    }
}

#[test]
fn eval_cannot_leave_stale_parameter_local_or_property_receiver_hints() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "main.php",
        "<?php\nclass A { function ping() {} }\nclass B extends A { function ping() {} }\nfunction marker() {}\nfunction parameter(A $x) { eval('$x = new B;'); $x->ping(); marker(); }\nfunction local() { $x = new A; eval('$x = new B;'); $x->ping(); marker(); }\nclass Box {\n function __construct(private A $p) {}\n function property() { eval('$this->p = new B;'); $this->p->ping(); marker(); }\n}\n",
    );
    let g = graph(&Options::new(dir.path()));
    for (label, line) in [("parameter", 5), ("local", 6), ("Box::property", 9)] {
        let caller = node(&g, "main.php", label, line);
        assert!(linked_at(
            &g,
            caller,
            node(&g, "main.php", "marker", 4),
            "calls",
            line
        ));
        for target in ["A::ping", "B::ping"] {
            let target_line = if target == "A::ping" { 2 } else { 3 };
            assert!(
                !linked_at(
                    &g,
                    caller,
                    node(&g, "main.php", target, target_line),
                    "calls",
                    line
                ),
                "{label} -> {target}"
            );
        }
    }
}

#[test]
fn namespaces_and_same_file_unconditional_helpers_preserve_polyfill_provenance() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "native.php",
        "<?php namespace Lib;\nfunction helper() {}\n",
    );
    put(
        dir.path(),
        "polyfill.php",
        "<?php namespace Lib {\nrequire __DIR__ . '/native.php';\nif (!function_exists('Lib\\\\helper')) { function helper() {} }\nfunction caller() { require_once __DIR__ . '/native.php'; helper(); }\n}\nnamespace Local {\nfunction helper() {}\nif ($runtime) { function helper() {} }\nfunction caller() { helper(); }\n}\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert!(linked_at(
        &g,
        node(&g, "polyfill.php", "Lib\\caller", 4),
        node(&g, "native.php", "Lib\\helper", 2),
        "calls",
        4
    ));
    let caller = node(&g, "polyfill.php", "Local\\caller", 9);
    assert!(linked_at(
        &g,
        caller,
        node(&g, "polyfill.php", "Local\\helper", 7),
        "calls",
        9
    ));
    assert!(!linked_at(
        &g,
        caller,
        node(&g, "polyfill.php", "Local\\helper", 8),
        "calls",
        9
    ));
}

#[test]
fn conditional_alternatives_in_other_files_and_namespaces_block_guessed_fallbacks() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "polyfill.php",
        "<?php\nrequire __DIR__ . '/native.php';\nif (!function_exists('helper')) { function helper() {} }\nfunction caller() { helper(); }\n",
    );
    put(
        dir.path(),
        "alternative.php",
        "<?php\nif ($runtime) { function helper() {} }\n",
    );
    put(
        dir.path(),
        "namespaced.php",
        "<?php namespace App;\nif ($runtime) { function helper() {} }\nfunction caller() { helper(); }\n",
    );
    let g = graph(&Options::new(dir.path()));
    for (file, label, line) in [
        ("polyfill.php", "caller", 4),
        ("namespaced.php", "App\\caller", 3),
    ] {
        let caller = node(&g, file, label, line);
        assert!(edges(&g, caller, "calls").is_empty());
        assert!(!linked_at(
            &g,
            caller,
            node(&g, "native.php", "helper", 2),
            "calls",
            line
        ));
    }
}

#[test]
fn conditional_function_ambiguity_stays_inside_its_independent_project() {
    let dir = TempDir::new().unwrap();
    put(dir.path(), "composer.json", "{}");
    put(dir.path(), "native.php", "<?php\nfunction helper() {}\n");
    put(
        dir.path(),
        "caller.php",
        "<?php\nfunction caller() { helper(); }\n",
    );
    put(dir.path(), "nested/composer.json", "{\"type\":\"project\"}");
    put(
        dir.path(),
        "nested/caller.php",
        "<?php\nif ($runtime) { function helper() {} }\nfunction inner() { helper(); }\n",
    );
    let g = graph(&Options::new(dir.path()));
    assert!(linked_at(
        &g,
        node(&g, "caller.php", "caller", 2),
        node(&g, "native.php", "helper", 2),
        "calls",
        2
    ));
    assert!(edges(&g, node(&g, "nested/caller.php", "inner", 3), "calls").is_empty());
}

#[test]
fn annotation_contexts_preserve_alias_fqn_union_intersection_fields_and_attributes() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "A.php",
        "<?php\nnamespace Lib;\nclass A { function ping() {} }\n",
    );
    put(
        dir.path(),
        "B.php",
        "<?php\nnamespace Lib;\ninterface B {}\n",
    );
    put(
        dir.path(),
        "Tag.php",
        "<?php\nnamespace Lib;\n#[\\Attribute] class Tag {}\n",
    );
    put(
        dir.path(),
        "Decoy.php",
        "<?php\nnamespace Other;\nclass Alias {}\n",
    );
    put(
        dir.path(),
        "main.php",
        "<?php\nnamespace App;\nuse Lib\\A as Alias;\nuse Lib\\Tag as Marker;\n#[Marker]\nclass Box {\n #[Marker] private ?Alias $field;\n function __construct(public Alias $promoted) {}\n}\n#[Marker]\nfunction caller(#[Marker] Alias|\\Lib\\B $x): Alias|\\Lib\\B { $x->ping(); }\nfunction both(Alias&\\Lib\\B $x): Alias { $x->ping(); }\nfunction builtins(int $x, string $y): mixed {}\ncaller();\n",
    );
    let graph = graph(&Options::new(dir.path()));
    let a = node(&graph, "A.php", "Lib\\A", 3);
    let b = node(&graph, "B.php", "Lib\\B", 3);
    let tag = node(&graph, "Tag.php", "Lib\\Tag", 3);
    let decoy = node(&graph, "Decoy.php", "Other\\Alias", 3);
    let caller = node(&graph, "main.php", "App\\caller", 10);
    for target in [a, b] {
        for context in ["parameter_type", "return_type"] {
            assert!(
                edges(&graph, caller, "references")
                    .iter()
                    .any(|edge| edge["target"] == target["id"]
                        && edge["context"] == context
                        && edge["source_location"] == "L11")
            );
        }
    }
    assert!(edges(&graph, caller, "calls").is_empty());
    assert!(
        !edges(&graph, caller, "references")
            .iter()
            .any(|e| e["target"] == decoy["id"])
    );
    assert!(
        edges(&graph, caller, "references")
            .iter()
            .any(|e| e["target"] == tag["id"] && e["context"] == "attribute")
    );
    let both = node(&graph, "main.php", "App\\both", 12);
    for target in [a, b] {
        assert!(
            edges(&graph, both, "references")
                .iter()
                .any(|e| e["target"] == target["id"] && e["context"] == "parameter_type")
        );
    }
    assert!(edges(&graph, both, "calls").is_empty());
    let class = node(&graph, "main.php", "App\\Box", 5);
    assert!(
        edges(&graph, class, "references")
            .iter()
            .any(|e| e["target"] == a["id"]
                && e["context"] == "field"
                && e["source_location"] == "L7")
    );
    assert!(
        edges(&graph, class, "references")
            .iter()
            .any(|e| e["target"] == tag["id"] && e["context"] == "attribute")
    );
    assert!(
        edges(
            &graph,
            node(&graph, "main.php", "App\\builtins", 13),
            "references"
        )
        .is_empty()
    );
}

#[test]
fn typed_properties_infer_only_single_alias_or_nullable_receivers_without_mutation() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "A.php",
        "<?php\nnamespace Lib;\nclass A { function ping() {} }\n",
    );
    put(
        dir.path(),
        "B.php",
        "<?php\nnamespace Lib;\nclass B extends A { function ping() {} }\n",
    );
    put(
        dir.path(),
        "main.php",
        "<?php\nnamespace App;\nuse Lib\\A as Alias;\nclass Box {\n private ?Alias $g;\n function good() { $this->g?->ping(); }\n}\nclass Promoted { function __construct(private Alias $g) {} function good() { $this->g->ping(); } }\nfunction top() {}\ntop();\n",
    );
    for (name, body) in [
        ("mutate", "$this->g = new \\Lib\\B; $this->g->ping();"),
        ("escape", "unknown($this->g); $this->g->ping();"),
        ("alias", "$alias =& $this->g; $this->g->ping();"),
        (
            "dynamic",
            "$this->{$name} = new \\Lib\\B; $this->g->ping();",
        ),
        ("unioned", "$this->g->ping();"),
        ("thisEscapes", "unknown($this); $this->g->ping();"),
        ("opaqueMethod", "$this->change(); $this->g->ping();"),
        ("staticCall", "$this->g->ping();"),
    ] {
        let ty = if name == "unioned" {
            "Alias|\\Lib\\B"
        } else {
            "?Alias"
        };
        let modifier = if name == "staticCall" { "static " } else { "" };
        put(
            dir.path(),
            &format!("{name}.php"),
            &format!(
                "<?php\nnamespace App;\nuse Lib\\A as Alias;\nclass Box_{name} {{\n private {ty} $g;\n {modifier}function run() {{ {body} }}\n}}\n"
            ),
        );
    }
    let graph = graph(&Options::new(dir.path()));
    let a = node(&graph, "A.php", "Lib\\A::ping", 3);
    let b = node(&graph, "B.php", "Lib\\B::ping", 3);
    for (label, line) in [("App\\Box::good", 6), ("App\\Promoted::good", 8)] {
        let caller = node(&graph, "main.php", label, line);
        assert!(linked_at(&graph, caller, a, "calls", line));
        assert!(!linked_at(&graph, caller, b, "calls", line));
        assert!(
            edges(&graph, caller, "calls")
                .iter()
                .all(|e| e["confidence"] == "INFERRED")
        );
    }
    for name in [
        "mutate",
        "escape",
        "alias",
        "dynamic",
        "unioned",
        "thisEscapes",
        "opaqueMethod",
        "staticCall",
    ] {
        assert!(
            edges(
                &graph,
                node(
                    &graph,
                    &format!("{name}.php"),
                    &format!("App\\Box_{name}::run"),
                    6
                ),
                "calls"
            )
            .is_empty(),
            "{name}"
        );
    }
}

#[test]
fn property_and_annotation_resolution_preserves_duplicate_ambiguity_and_exclusions() {
    let dir = TempDir::new().unwrap();
    for path in ["first.php", "second.php"] {
        put(
            dir.path(),
            path,
            "<?php\nnamespace Lib;\nclass A { function ping() {} }\n",
        );
    }
    put(
        dir.path(),
        "main.php",
        "<?php\nuse Lib\\A as Alias;\nclass Box { private Alias $g; function run() { $this->g->ping(); } }\nfunction annotation(Alias $x): Alias {}\n",
    );
    let mut options = Options::new(dir.path());
    let g = graph(&options);
    assert!(edges(&g, node(&g, "main.php", "Box::run", 3), "calls").is_empty());
    assert!(edges(&g, node(&g, "main.php", "annotation", 4), "references").is_empty());
    options.exclude_file.push("second.php".into());
    let g = graph(&options);
    assert!(linked_at(
        &g,
        node(&g, "main.php", "Box::run", 3),
        node(&g, "first.php", "Lib\\A::ping", 3),
        "calls",
        3
    ));
    for context in ["parameter_type", "return_type"] {
        assert!(
            edges(&g, node(&g, "main.php", "annotation", 4), "references")
                .iter()
                .any(|e| e["context"] == context
                    && e["target"] == node(&g, "first.php", "Lib\\A", 3)["id"])
        );
    }
}

#[test]
fn constructor_or_other_method_mutation_cannot_leave_a_stale_property_receiver() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "main.php",
        "<?php\nclass A { function ping() {} }\nclass B extends A { function ping() {} }\nclass Constructed {\n private A $p;\n function __construct() { $this->p = new B; }\n function run() { $this->p->ping(); }\n}\nclass Mutated {\n private A $p;\n function replace() { $this->p = new B; }\n function run() { $this->p->ping(); }\n}\nclass Promoted {\n function __construct(private A $p) {}\n function run() { $this->p->ping(); }\n}\n",
    );
    let g = graph(&Options::new(dir.path()));
    let a = node(&g, "main.php", "A::ping", 2);
    let b = node(&g, "main.php", "B::ping", 3);
    for (label, line) in [("Constructed::run", 7), ("Mutated::run", 12)] {
        let run = node(&g, "main.php", label, line);
        assert!(!linked_at(&g, run, a, "calls", line));
        assert!(!linked_at(&g, run, b, "calls", line));
    }
    let promoted = node(&g, "main.php", "Promoted::run", 16);
    assert!(linked_at(&g, promoted, a, "calls", 16));
    assert!(!linked_at(&g, promoted, b, "calls", 16));
    assert!(
        edges(&g, promoted, "calls")
            .iter()
            .all(|edge| edge["confidence"] == "INFERRED")
    );
}

#[test]
fn dnf_byref_variadic_and_return_types_reference_declarations_without_guessing_call_results() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "types.php",
        "<?php\nnamespace Lib;\nclass A { function ping() {} }\ninterface B {}\nclass C { function ping() {} }\n",
    );
    put(
        dir.path(),
        "main.php",
        "<?php\nuse Lib\\A as Alias;\nfunction dnf((Alias&\\Lib\\B)|\\Lib\\C $x) { $x->ping(); }\nfunction variadic(Alias &$x, \\Lib\\B ...$rest): ?Alias {}\nfunction make(): Alias { return new Alias; }\nfunction caller() { make()->ping(); }\ncaller();\n",
    );
    let g = graph(&Options::new(dir.path()));
    let dnf = node(&g, "main.php", "dnf", 3);
    for (label, line) in [("Lib\\A", 3), ("Lib\\B", 4), ("Lib\\C", 5)] {
        assert!(edges(&g, dnf, "references").iter().any(|edge| {
            edge["context"] == "parameter_type"
                && edge["target"] == node(&g, "types.php", label, line)["id"]
                && edge["source_location"] == "L3"
        }));
    }
    assert!(edges(&g, dnf, "calls").is_empty());
    let variadic = node(&g, "main.php", "variadic", 4);
    assert_eq!(edges(&g, variadic, "references").len(), 3);
    let caller = node(&g, "main.php", "caller", 6);
    let make = node(&g, "main.php", "make", 5);
    assert!(linked_at(&g, caller, make, "calls", 6));
    for (label, line) in [("Lib\\A::ping", 3), ("Lib\\C::ping", 5)] {
        assert!(!linked_at(
            &g,
            caller,
            node(&g, "types.php", label, line),
            "calls",
            6
        ));
    }
}

#[test]
fn malformed_type_and_property_sources_never_publish_a_partial_graph() {
    let dir = TempDir::new().unwrap();
    put(
        dir.path(),
        "main.php",
        "<?php\nclass A {}\nfunction before(A $x): A { return $x; }\n",
    );
    let options = Options::new(dir.path());
    graph(&options);
    let previous = fs::read(dir.path().join("graphify-out/graph.json")).unwrap();
    for malformed in [
        "<?php\nclass A {}\nfunction before(A $x): A { return $x; }\nfunction broken(A| $x) {}\n",
        "<?php\nclass A {}\nclass Box { private ?A $g function broken() {} }\n",
        "<?php\n#[A(] function broken(): A {}\n",
    ] {
        put(dir.path(), "main.php", malformed);
        let error = extract(&options).unwrap_err().to_string();
        assert!(error.contains("main.php"), "{error}");
        assert_eq!(
            fs::read(dir.path().join("graphify-out/graph.json")).unwrap(),
            previous
        );
    }
}
