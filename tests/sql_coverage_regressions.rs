use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

#[test]
fn statement_only_coverage_is_explicit_cached_and_worker_independent() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    for name in ["a", "b"] {
        fs::write(root.path().join(format!("{name}.sql")), format!(
            "CREATE TABLE {name}(id INT);\nDELIMITER //\nCREATE TRIGGER {name}_changed AFTER UPDATE ON {name} FOR EACH ROW BEGIN SET @value = NEW.id; END//\nDELIMITER ;\n"
        )).unwrap();
    }
    let mut expected = None;
    for (workers, cached) in [(1, false), (1, true), (4, true), (4, false)] {
        let mut options = Options::new(root.path());
        options.sql_dialect = "mysql".into();
        options.workers = workers;
        options.cache_dir = cached.then(|| cache.path().to_owned());
        let stats = extract(&options).unwrap();
        assert_eq!(stats.sql_statement_only, 2);
        let bytes = fs::read(root.path().join("graphify-out/graph.json")).unwrap();
        if let Some(previous) = &expected {
            assert_eq!(previous, &bytes);
        } else {
            expected = Some(bytes.clone());
        }
        let g: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(g["graph"]["stats"]["sql_statement_only"], 2);
        let statements: Vec<_> = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] == "sql_statement")
            .collect();
        assert_eq!(statements.len(), 2);
        for node in statements {
            assert_eq!(node["extraction_coverage"], "statement_only");
            assert_eq!(node["source_location"], "L3");
            assert!(
                g["links"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["target"] == node["id"]
                        && e["relation"] == "contains"
                        && e["source_file"] == node["source_file"]
                        && e["source_location"] == "L3")
            );
            assert!(
                !g["links"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["source"] == node["id"])
            );
        }
        for node in g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] != "sql_statement")
        {
            assert!(node.get("extraction_coverage").is_none());
        }
    }
    // Removing one source invalidates both its facts and its coverage counter.
    fs::remove_file(root.path().join("b.sql")).unwrap();
    let mut options = Options::new(root.path());
    options.sql_dialect = "mysql".into();
    options.cache_dir = Some(cache.path().to_owned());
    assert_eq!(extract(&options).unwrap().sql_statement_only, 1);
    let bytes = fs::read(root.path().join("graphify-out/graph.json")).unwrap();
    options.cache_dir = None;
    assert_eq!(extract(&options).unwrap().sql_statement_only, 1);
    assert_eq!(
        bytes,
        fs::read(root.path().join("graphify-out/graph.json")).unwrap()
    );
}
