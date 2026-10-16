use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

#[test]
fn mysql_vendor_permissions_are_counted_without_losing_schema_edges() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("permissions.sql"),
        r#"
CREATE TABLE parent (id INT PRIMARY KEY);
CREATE USER 'gha'@'%' IDENTIFIED WITH mysql_native_password BY 'password';
GRANT ALL PRIVILEGES ON *.* TO 'gha'@'%';
FLUSH PRIVILEGES;
CREATE TABLE child (parent_id INT, FOREIGN KEY (parent_id) REFERENCES parent(id));
"#,
    )
    .unwrap();
    let stats = extract(&Options::new(root.path())).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.ignored_sql_statements, 3);
    let graph: Value =
        serde_json::from_slice(&fs::read(root.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    let nodes = graph["nodes"].as_array().unwrap();
    let parent = nodes
        .iter()
        .find(|n| n["type"] == "table" && n["label"] == "parent")
        .unwrap();
    let child = nodes
        .iter()
        .find(|n| n["type"] == "table" && n["label"] == "child")
        .unwrap();
    assert!(
        graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == child["id"]
                && e["target"] == parent["id"]
                && e["relation"] == "references")
    );
    assert!(!nodes.iter().any(|n| n["label"] == "gha"));
}

#[test]
fn malformed_or_unhandled_account_statements_fail_without_replacing_graph() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("schema.sql");
    fs::write(&path, "CREATE TABLE stable(id INT);").unwrap();
    extract(&Options::new(root.path())).unwrap();
    let output = root.path().join("graphify-out/graph.json");
    let previous = fs::read(&output).unwrap();
    for invalid in [
        "CREATE USER 'gha'@ IDENTIFIED BY 'password';",
        "CREATE USER 'gha'@'%' IDENTIFIED WITH plugin BY;",
        "CREATE USER 'gha'@'%' trailing_garbage;",
        "GRANT ALL PRIVILEGES ON *.* TO;",
        "GRANT ALL PRIVILEGES ON *.* TO 'gha'@'%' nonsense;",
        "FLUSH PRIVILEGES nonsense;",
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            extract(&Options::new(root.path())).is_err(),
            "accepted {invalid}"
        );
        assert_eq!(fs::read(&output).unwrap(), previous);
    }
}

#[test]
fn mysql_account_lists_and_identified_by_are_validated_completely() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("accounts.sql"), "CREATE USER IF NOT EXISTS 'one'@'localhost' IDENTIFIED BY 'a;quoted', 'two'@'%'; GRANT ALL ON app.* TO 'one'@'localhost', 'two'@'%' WITH GRANT OPTION;").unwrap();
    let stats = extract(&Options::new(root.path())).unwrap();
    assert_eq!(stats.ignored_sql_statements, 2);
    assert_eq!(stats.nodes, 1);
    assert_eq!(stats.edges, 0);
}
