use graphify_extractor::{ExportProfile, Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn run(root: &std::path::Path, profile: ExportProfile, workers: usize) -> Value {
    let mut options = Options::new(root);
    options.export_profile = profile;
    options.workers = workers;
    extract(&options).unwrap();
    serde_json::from_slice(&fs::read(root.join("graphify-out/graph.json")).unwrap()).unwrap()
}
fn node<'a>(graph: &'a Value, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == label)
        .unwrap()
}
fn has(graph: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    graph["links"].as_array().unwrap().iter().any(|e| {
        e["source"] == source["id"] && e["target"] == target["id"] && e["relation"] == relation
    })
}
#[test]
fn graphify_projection_preserves_identity_targets_and_provenance() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("library.php"),"<?php\nclass Base { public function act() {} }\nclass Child extends Base { public function act() {} }\nfunction target() {}\n").unwrap();
    fs::write(dir.path().join("client.php"),"<?php\nrequire __DIR__ . '/library.php';\nfunction caller(Base $x) { $x->act(); target(); }\n").unwrap();
    let native = run(dir.path(), ExportProfile::Native, 1);
    let graph = run(dir.path(), ExportProfile::Graphify, 1);
    assert_eq!(graph, run(dir.path(), ExportProfile::Graphify, 8));
    let original = node(&native, "Base::act");
    let projected = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == original["id"])
        .unwrap();
    assert_eq!(projected["label"], ".act()");
    assert_eq!(projected["qualified_name"], "Base::act");
    assert_eq!(projected["source_location"], "L2");
    assert!(has(&graph, node(&graph, "Base"), projected, "method"));
    assert!(!has(&graph, node(&graph, "Child"), projected, "method"));
    assert!(has(
        &graph,
        node(&graph, "library.php"),
        node(&graph, "target"),
        "contains"
    ));
    assert!(has(
        &graph,
        node(&graph, "client.php"),
        node(&graph, "library.php"),
        "requires"
    ));
    let call = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["source"] == node(&graph, "caller")["id"]
                && e["target"] == original["id"]
                && e["relation"] == "calls"
        })
        .unwrap();
    assert_eq!(call["source_file"], "client.php");
    assert_eq!(call["source_location"], "L3");
    let mut native_ids: Vec<_> = native["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    native_ids.sort();
    let mut ids: Vec<_> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(native_ids, ids);
}
#[test]
fn contexts_between_identical_endpoints_survive_both_export_profiles() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("api.php"),
        "<?php\nclass Item {}\nfunction convert(Item $item): Item { return $item; }\n",
    )
    .unwrap();
    for profile in [ExportProfile::Native, ExportProfile::Graphify] {
        let graph = run(dir.path(), profile, 2);
        let source = node(&graph, "convert");
        let target = node(&graph, "Item");
        for context in ["parameter_type", "return_type"] {
            let refs: Vec<_> = graph["links"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| {
                    e["source"] == source["id"]
                        && e["target"] == target["id"]
                        && e["relation"] == "references"
                        && e["context"] == context
                })
                .collect();
            assert_eq!(refs.len(), 1, "missing or duplicated {context}");
            assert_eq!(refs[0]["source_location"], "L3");
        }
    }
}

#[test]
fn cached_raw_contexts_can_be_exported_in_both_profiles() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        project.path().join("model.php"),
        "<?php\nclass Item {}\nfunction convert(Item $item): Item { return $item; }\n",
    )
    .unwrap();
    fs::write(
        project.path().join("client.php"),
        "<?php\nrequire __DIR__ . '/model.php';\nconvert(new Item());\n",
    )
    .unwrap();
    fs::write(
        project.path().join("schema.sql"),
        "CREATE TABLE items (id INT);\nCREATE VIEW listed AS SELECT id FROM items;\n",
    )
    .unwrap();
    let mut options = Options::new(project.path());
    options.cache_dir = Some(cache.path().to_owned());
    for profile in [
        ExportProfile::Native,
        ExportProfile::Graphify,
        ExportProfile::Native,
    ] {
        options.export_profile = profile;
        options.workers = 4;
        extract(&options).unwrap();
        let cached: Value = serde_json::from_slice(
            &fs::read(project.path().join("graphify-out/graph.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(cached, run(project.path(), profile, 4));
        let source = node(&cached, "convert");
        let target = node(&cached, "Item");
        for context in ["parameter_type", "return_type"] {
            assert!(
                cached["links"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["source"] == source["id"]
                        && edge["target"] == target["id"]
                        && edge["context"] == context)
            );
        }
        assert!(
            cached["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["relation"] == "reads_from" && edge["source_location"] == "L2")
        );
    }
}

#[test]
fn construction_and_trait_projection_keep_native_semantics_and_callsite() {
    let project = TempDir::new().unwrap();
    fs::write(project.path().join("model.php"), "<?php\ntrait Shared {}\ntrait Decoy {}\nclass Item { use Shared; }\nfunction build() { return new Item(); }\n").unwrap();
    let graph = run(project.path(), ExportProfile::Graphify, 2);
    let item = node(&graph, "Item");
    let shared = node(&graph, "Shared");
    let decoy = node(&graph, "Decoy");
    assert!(has(&graph, item, shared, "mixes_in"));
    assert!(!has(&graph, item, decoy, "mixes_in"));
    let construct = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["source"] == node(&graph, "build")["id"] && edge["target"] == item["id"])
        .unwrap();
    assert_eq!(construct["relation"], "calls");
    assert_eq!(construct["original_relation"], "instantiates");
    assert_eq!(construct["context"], "call");
    assert_eq!(construct["source_file"], "model.php");
    assert_eq!(construct["source_location"], "L5");
    let mixin = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| {
            edge["source"] == item["id"]
                && edge["target"] == shared["id"]
                && edge["relation"] == "mixes_in"
        })
        .unwrap();
    assert_eq!(mixin["original_relation"], "uses_trait");
    assert_eq!(mixin["source_location"], "L4");
}
