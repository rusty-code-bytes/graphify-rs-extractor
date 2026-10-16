use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

const SERVICE: &str =
    "<?php\nnamespace Vendor;\nclass Service {\n    static function run() {}\n}\n";
const CALLER: &str = "<?php\nnamespace Client;\nuse Vendor\\Service as Selected;\nfunction invoke() {\n    Selected::run();\n}\nclass Child extends Selected {}\n";

fn put(root: &Path, file: &str, source: &str) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn graph(options: &Options) -> Value {
    extract(options).unwrap();
    serde_json::from_slice(&fs::read(options.root.join("graphify-out/graph.json")).unwrap())
        .unwrap()
}

fn node<'a>(graph: &'a Value, file: &str, label: &str, line: usize) -> &'a Value {
    let node = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["source_file"] == file && node["label"] == label)
        .unwrap_or_else(|| panic!("missing {file}: {label}"));
    assert_eq!(node["source_location"], format!("L{line}"));
    node
}

fn edge(graph: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    let Some(edge) = graph["links"].as_array().unwrap().iter().find(|edge| {
        edge["source"] == source["id"]
            && edge["target"] == target["id"]
            && edge["relation"] == relation
    }) else {
        return false;
    };
    assert_eq!(edge["source_file"], source["source_file"]);
    assert_eq!(
        edge["source_location"],
        if relation == "calls" { "L5" } else { "L7" }
    );
    true
}

fn check_selection(graph: &Value, caller: &str, selected: Option<&str>, decoy: &str) {
    let invoke = node(graph, caller, "Client\\invoke", 4);
    let child = node(graph, caller, "Client\\Child", 7);
    for (source, label, line, relation) in [
        (invoke, "Vendor\\Service::run", 4, "calls"),
        (child, "Vendor\\Service", 3, "extends"),
    ] {
        let wrong = node(graph, decoy, label, line);
        assert!(!edge(graph, source, wrong, relation));
        if let Some(file) = selected {
            assert!(edge(
                graph,
                source,
                node(graph, file, label, line),
                relation
            ));
        } else {
            assert!(
                !graph["links"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| { edge["source"] == source["id"] && edge["relation"] == relation }),
                "{relation} must remain unresolved after its loader selection is hidden or missing"
            );
        }
    }
}

#[test]
fn installed_classmap_selection_never_rebinds_after_exclusion_or_deletion() {
    for workers in [1, 4] {
        for state in ["indexed", "excluded", "missing"] {
            let dir = TempDir::new().unwrap();
            let root = dir.path();
            put(
                root,
                "composer.json",
                r#"{"autoload":{"psr-4":{"Vendor\\":"a_decoy/"}}}"#,
            );
            put(
                root,
                "vendor/composer/autoload_classmap.php",
                "<?php return ['Vendor\\\\Service' => __DIR__ . '/../../z_runtime/Service.php'];",
            );
            put(root, "a_decoy/Service.php", SERVICE);
            put(root, "caller.php", CALLER);
            if state != "missing" {
                put(root, "z_runtime/Service.php", SERVICE);
            }
            let mut options = Options::new(root);
            options.workers = workers;
            if state == "excluded" {
                options.exclude_file.push("z_runtime/Service.php".into());
            }
            let graph = graph(&options);
            check_selection(
                &graph,
                "caller.php",
                (state == "indexed").then_some("z_runtime/Service.php"),
                "a_decoy/Service.php",
            );
            if state != "indexed" {
                assert!(
                    !graph["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|node| { node["source_file"] == "z_runtime/Service.php" })
                );
            }
            assert_eq!(
                root.join("z_runtime/Service.php").exists(),
                state != "missing"
            );
        }
    }
}

#[test]
fn installed_psr_selection_blocks_excluded_first_directory_but_missing_paths_can_fall_through() {
    for mode in ["psr-4", "psr-0"] {
        for state in ["indexed", "excluded", "missing"] {
            let dir = TempDir::new().unwrap();
            let root = dir.path();
            put(
                root,
                "composer.json",
                &format!(r#"{{"autoload":{{"{mode}":{{"Vendor\\":["first/","second/"]}}}}}}"#),
            );
            let suffix = if mode == "psr-4" {
                "Service.php"
            } else {
                "Vendor/Service.php"
            };
            let first = format!("first/{suffix}");
            let second = format!("second/{suffix}");
            if state != "missing" {
                put(root, &first, SERVICE);
            }
            put(root, &second, SERVICE);
            put(root, "decoy/Service.php", SERVICE);
            put(root, "caller.php", CALLER);
            let mut options = Options::new(root);
            if state == "excluded" {
                options.exclude_file.push(first.clone().into());
            }
            let graph = graph(&options);
            let selected = match state {
                "indexed" => Some(first.as_str()),
                "missing" => Some(second.as_str()),
                _ => None,
            };
            check_selection(&graph, "caller.php", selected, "decoy/Service.php");
            if state != "missing" {
                for (source, label, line, relation) in [
                    ("Client\\invoke", "Vendor\\Service::run", 4, "calls"),
                    ("Client\\Child", "Vendor\\Service", 3, "extends"),
                ] {
                    let source_line = if relation == "calls" { 4 } else { 7 };
                    assert!(
                        !edge(
                            &graph,
                            node(&graph, "caller.php", source, source_line),
                            node(&graph, &second, label, line),
                            relation,
                        ),
                        "mode={mode}, state={state}"
                    );
                }
            }
        }
    }
}

#[test]
fn psr_case_variants_cannot_discard_an_unknown_spelling_and_choose_a_visible_duplicate() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Vendor\\":"runtime/"}}}"#,
    );
    put(root, "runtime/Service.php", SERVICE);
    put(
        root,
        "decoy.php",
        &SERVICE.replace("class Service", "class SERVICE"),
    );
    put(root, "caller.php", CALLER);
    let graph = graph(&Options::new(root));
    for (label, line, relation) in [
        ("Client\\invoke", 4, "calls"),
        ("Client\\Child", 7, "extends"),
    ] {
        let source = node(&graph, "caller.php", label, line);
        assert!(
            !graph["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| { edge["source"] == source["id"] && edge["relation"] == relation })
        );
    }
    node(&graph, "runtime/Service.php", "Vendor\\Service::run", 4);
    node(&graph, "decoy.php", "Vendor\\SERVICE::run", 4);
}

#[test]
fn unregistered_nested_library_manifest_cannot_override_the_root_loader() {
    for workers in [1, 4] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(root, "composer.json", "{}");
        put(
            root,
            "vendor/composer/autoload_classmap.php",
            "<?php return ['Vendor\\\\Service' => __DIR__ . '/../../z_runtime/Service.php'];",
        );
        put(
            root,
            "vendor/autoload.php",
            "<?php require __DIR__ . '/../z_runtime/Service.php';\n",
        );
        put(
            root,
            "bootstrap.php",
            "<?php\nrequire __DIR__ . '/vendor/autoload.php';\nrequire __DIR__ . '/lib/package/caller.php';\n\\Client\\invoke();\n",
        );
        put(root, "z_runtime/Service.php", SERVICE);
        put(root, "lib/package/local/Service.php", SERVICE);
        put(root, "lib/package/caller.php", CALLER);
        put(
            root,
            "lib/package/composer.json",
            r#"{"type":"library","autoload":{"psr-4":{"Vendor\\":"local/"}}}"#,
        );
        let mut options = Options::new(root);
        options.workers = workers;
        let graph = graph(&options);
        check_selection(
            &graph,
            "lib/package/caller.php",
            Some("z_runtime/Service.php"),
            "lib/package/local/Service.php",
        );
    }
}

#[test]
fn generated_maps_do_not_activate_nested_libraries_but_explicit_projects_keep_their_loader() {
    for kind in ["library", "project"] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        put(
            root,
            "composer.json",
            r#"{"type":"library","autoload":{"psr-4":{"Vendor\\":"runtime/"}}}"#,
        );
        put(root, "runtime/Service.php", SERVICE);
        put(root, "caller.php", CALLER);
        put(root, "nested/local/Service.php", SERVICE);
        put(root, "nested/caller.php", CALLER);
        put(
            root,
            "nested/composer.json",
            &format!(r#"{{"type":"{kind}","config":{{"classmap-authoritative":true}}}}"#),
        );
        put(
            root,
            "nested/vendor/composer/autoload_classmap.php",
            "<?php return ['Vendor\\\\Service' => __DIR__ . '/../../local/Service.php'];",
        );
        let graph = graph(&Options::new(root));
        if kind == "project" {
            check_selection(
                &graph,
                "nested/caller.php",
                Some("nested/local/Service.php"),
                "runtime/Service.php",
            );
        } else {
            check_selection(
                &graph,
                "nested/caller.php",
                Some("runtime/Service.php"),
                "nested/local/Service.php",
            );
        }
        check_selection(
            &graph,
            "caller.php",
            Some("runtime/Service.php"),
            "nested/local/Service.php",
        );
    }
}

#[test]
fn a_standalone_library_scan_keeps_its_generated_loader_metadata() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    put(
        root,
        "composer.json",
        r#"{"type":"library","autoload":{"psr-4":{"Vendor\\":"decoy/"}}}"#,
    );
    put(
        root,
        "vendor/composer/autoload_classmap.php",
        "<?php return ['Vendor\\\\Service' => __DIR__ . '/../../runtime/Service.php'];",
    );
    put(root, "runtime/Service.php", SERVICE);
    put(root, "decoy/Service.php", SERVICE);
    put(root, "caller.php", CALLER);
    check_selection(
        &graph(&Options::new(root)),
        "caller.php",
        Some("runtime/Service.php"),
        "decoy/Service.php",
    );
}
