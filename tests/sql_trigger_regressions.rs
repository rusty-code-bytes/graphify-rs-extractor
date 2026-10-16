use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn node<'a>(graph: &'a Value, kind: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == kind && node["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label}: {graph}"))
}

fn edge(graph: &Value, source: &Value, target: &Value, relation: &str) -> bool {
    graph["links"].as_array().unwrap().iter().any(|edge| {
        edge["source"] == source["id"]
            && edge["target"] == target["id"]
            && edge["relation"] == relation
    })
}

#[test]
fn mysql_trigger_insert_value_keeps_following_schema_and_provenance() {
    let dir = TempDir::new().unwrap();
    let sql = "CREATE TABLE source(id INT);\n\
CREATE TABLE decoy(id INT);\n\
DELIMITER //\n\
CREATE TRIGGER record_update AFTER UPDATE ON source FOR EACH ROW BEGIN\n\
  INSERT INTO audit_log(id) VALUE(NEW.id);\n\
END//\n\
DELIMITER ;\n\
CREATE TABLE child(id INT REFERENCES source(id));\n";
    fs::write(dir.path().join("migration.sql"), sql).unwrap();
    let mut options = Options::new(dir.path());
    options.sql_dialect = "mysql".into();
    let stats = extract(&options).unwrap();
    assert_eq!(stats.skipped_sql_data_statements, 0);
    assert_eq!(stats.ignored_sql_statements, 0);
    let graph: Value =
        serde_json::from_slice(&fs::read(dir.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    let source = node(&graph, "table", "source");
    let decoy = node(&graph, "table", "decoy");
    let child = node(&graph, "table", "child");
    assert_eq!(source["source_location"], "L1");
    assert_eq!(child["source_location"], "L8");
    assert!(edge(&graph, child, source, "references"));
    assert!(!edge(&graph, child, decoy, "references"));
    let reference = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["source"] == child["id"] && edge["relation"] == "references")
        .unwrap();
    assert_eq!(reference["source_file"], "migration.sql");
    assert_eq!(reference["source_location"], "L8");
    let trigger = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| {
            node["type"] == "sql_statement"
                && node["label"]
                    .as_str()
                    .unwrap()
                    .starts_with("CREATE TRIGGER record_update ")
        })
        .unwrap();
    assert_eq!(trigger["source_file"], "migration.sql");
    assert_eq!(trigger["source_location"], "L4");
    assert!(edge(
        &graph,
        node(&graph, "file", "migration.sql"),
        trigger,
        "contains"
    ));
    // Trigger bodies currently have explicit parsed-statement coverage only.
    assert!(
        !graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| { edge["source"] == trigger["id"] && edge["relation"] != "contains" })
    );
}

#[test]
fn mysql_conditional_trigger_value_and_delimiter_reset_preserve_existing_edges() {
    let dir = TempDir::new().unwrap();
    // An anonymized migration: surrounding statements exercise the same client
    // delimiter and conditional-body structure as the reported failure.
    let sql = r#"CREATE TABLE app.source(id INT, amount DECIMAL(12,2), issuedate DATETIME,
deleted INT, status INT, updated_at TIMESTAMP);
CREATE TABLE other.source(id INT);
CREATE TABLE app.child(id INT);
USE app;
UPDATE source SET status = 2 WHERE status = 1;
DROP TRIGGER IF EXISTS track_update;
ALTER TABLE source MODIFY COLUMN updated_at TIMESTAMP NOT NULL
  DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP;
DELIMITER //
CREATE TRIGGER track_update AFTER UPDATE ON source FOR EACH ROW
BEGIN
  DELETE FROM cache WHERE id = NEW.id;
  UPDATE report SET status = 2 WHERE OLD.issuedate BETWEEN dateFrom AND dateTo;
  IF ifnull(NEW.amount, 0) <> ifnull(OLD.amount, 0) OR NEW.deleted <> OLD.deleted THEN
    INSERT INTO calculation(id, issuedate, ts) VALUE (NEW.id, NEW.issuedate, NOW());
    INSERT INTO calculation(id, issuedate, ts) vAlUe /* VALUE is a synonym */
      (NEW.id, NEW.issuedate, NOW());
  END IF;
  IF NEW.status <> OLD.status THEN
    CALL sp_recalculate(NEW.id);
  END IF;
  IF NEW.deleted = 1 THEN
    INSERT INTO archive(id, amount) SELECT NEW.id, NEW.amount;
  END IF;
END
//
DELIMITER ;
INSERT INTO calculation(id) VALUE (1);
UPDATE report SET status = 3;
ALTER TABLE child ADD CONSTRAINT child_source FOREIGN KEY(id) REFERENCES source(id);
CREATE VIEW changed_sources AS SELECT id FROM source;
"#;
    fs::write(dir.path().join("migration.sql"), sql).unwrap();
    let stats = extract(&Options::new(dir.path())).unwrap();
    assert_eq!(stats.skipped_sql_data_statements, 1);
    assert_eq!(stats.ignored_sql_statements, 3);
    let graph: Value =
        serde_json::from_slice(&fs::read(dir.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    let source = node(&graph, "table", "app.source");
    let decoy = node(&graph, "table", "other.source");
    let view = node(&graph, "view", "changed_sources");
    let migration = node(&graph, "migration", "ALTER child@31");
    assert!(edge(&graph, view, source, "reads_from"));
    assert!(!edge(&graph, view, decoy, "reads_from"));
    assert!(edge(&graph, migration, source, "references"));
    assert!(!edge(&graph, migration, decoy, "references"));
    assert!(edge(
        &graph,
        migration,
        node(&graph, "table", "app.child"),
        "alters"
    ));
    assert!(!edge(&graph, migration, source, "alters"));
    for (owner, relation, line) in [
        (view, "reads_from", "L32"),
        (migration, "references", "L31"),
    ] {
        assert_eq!(owner["source_location"], line);
        let reference = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["source"] == owner["id"] && edge["relation"] == relation)
            .unwrap();
        assert_eq!(reference["target"], source["id"]);
        assert_eq!(reference["source_file"], "migration.sql");
        assert_eq!(reference["source_location"], line);
    }
    let statements: Vec<_> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "sql_statement")
        .collect();
    assert_eq!(statements.len(), 2);
    assert_eq!(
        node(
            &graph,
            "sql_statement",
            "DROP TRIGGER IF EXISTS track_update@7"
        )["source_location"],
        "L7"
    );
    let trigger = statements
        .iter()
        .find(|statement| {
            statement["label"]
                .as_str()
                .unwrap()
                .starts_with("CREATE TRIGGER track_update ")
        })
        .unwrap();
    assert_eq!(trigger["source_location"], "L11");
}

#[test]
fn malformed_mysql_trigger_value_rows_fail_closed() {
    for body in [
        "INSERT INTO audit_log(id) VALUE;",
        "INSERT INTO audit_log(id) VALUE (NEW.id,);",
        "INSERT INTO audit_log(id) VALUE (NEW.id) nonsense;",
        "INSERT INTO audit_log(id) VALUE (NEW.id); INSERT INTO audit_log(id) VALUE (",
    ] {
        let dir = TempDir::new().unwrap();
        let mut options = Options::new(dir.path());
        options.sql_dialect = "mysql".into();
        fs::write(
            dir.path().join("known.sql"),
            "CREATE TABLE retained(id INT);",
        )
        .unwrap();
        extract(&options).unwrap();
        let graph_path = dir.path().join("graphify-out/graph.json");
        let previous_graph = fs::read(&graph_path).unwrap();
        fs::write(
            dir.path().join("broken.sql"),
            format!(
                "DELIMITER //\nCREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN\n{body}\nEND//\nDELIMITER ;\nCREATE TABLE after_failure(id INT);"
            ),
        )
        .unwrap();
        let error = extract(&options).unwrap_err();
        assert!(format!("{error:#}").contains("broken.sql"));
        assert_eq!(fs::read(graph_path).unwrap(), previous_graph);
    }
}

#[test]
fn mysql_trigger_local_declarations_keep_following_schema_and_provenance() {
    let dir = TempDir::new().unwrap();
    let sql = "CREATE TABLE app.source(id INT);\n\
CREATE TABLE other.source(id INT);\n\
USE app;\n\
DELIMITER //\n\
CREATE TRIGGER balance_update AFTER UPDATE ON source FOR EACH ROW BEGIN\n\
  DECLARE vBalance DECIMAL(18,2);\n\
  DECLARE vDate DATE;\n\
  DECLARE vOldCase, vNewCase INT DEFAULT 0;\n\
  SET vBalance = NEW.id;\n\
END//\n\
DELIMITER ;\n\
CREATE TABLE child(id INT REFERENCES source(id));\n\
CREATE VIEW source_ids AS SELECT id FROM source;\n";
    fs::write(dir.path().join("migration.sql"), sql).unwrap();
    let mut options = Options::new(dir.path());
    options.sql_dialect = "mysql".into();
    let stats = extract(&options).unwrap();
    assert_eq!(stats.skipped_sql_data_statements, 0);
    assert_eq!(stats.ignored_sql_statements, 1);
    let graph: Value =
        serde_json::from_slice(&fs::read(dir.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    let source = node(&graph, "table", "app.source");
    let decoy = node(&graph, "table", "other.source");
    for (owner, relation, line) in [
        (node(&graph, "table", "child"), "references", "L12"),
        (node(&graph, "view", "source_ids"), "reads_from", "L13"),
    ] {
        assert_eq!(owner["source_location"], line);
        assert!(edge(&graph, owner, source, relation));
        assert!(!edge(&graph, owner, decoy, relation));
        let reference = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["source"] == owner["id"] && edge["relation"] == relation)
            .unwrap();
        assert_eq!(reference["target"], source["id"]);
        assert_eq!(reference["source_file"], "migration.sql");
        assert_eq!(reference["source_location"], line);
    }
    let statements: Vec<_> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "sql_statement")
        .collect();
    assert_eq!(statements.len(), 1);
    let trigger = statements[0];
    assert!(trigger["label"].as_str().unwrap().starts_with(
        "CREATE TRIGGER balance_update AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance DECIMAL(18,2); DECLARE vDate DATE;"
    ));
    assert_eq!(trigger["source_file"], "migration.sql");
    assert_eq!(trigger["source_location"], "L5");
    assert!(edge(
        &graph,
        node(&graph, "file", "migration.sql"),
        trigger,
        "contains"
    ));
    assert!(
        !graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == trigger["id"])
    );
}

#[test]
fn mysql_trigger_local_declarations_validate_realistic_nested_body() {
    let dir = TempDir::new().unwrap();
    // Anonymized from the reported migration; retain the syntax after DECLARE
    // so a declaration fix cannot hide the next body parsing failure.
    let sql = r#"CREATE TABLE app.source(id INT, amount DECIMAL(18,2), issuedate DATE, status INT);
CREATE TABLE other.source(id INT);
USE app;
DELIMITER //
CREATE TRIGGER balance_update AFTER UPDATE ON source FOR EACH ROW BEGIN
  DECLARE vBalance DECIMAL(18,2);
  DECLARE vDate DATE DEFAULT current_date();
  DECLARE vOldCase, vNewCase INT DEFAULT (1 + 2);
  DELETE FROM cache WHERE id = NEW.id;
  UPDATE report SET status = 2 WHERE OLD.issuedate BETWEEN dateFrom AND dateTo;
  IF NEW.amount <> OLD.amount THEN
    UPDATE calculation SET amount = NEW.amount WHERE id = NEW.id;
  END IF;
  SET vOldCase = 0;
  IF fx_version() <> 1 THEN
    SET vOldCase = 1, vNewCase = 2;
  ELSE
    SELECT fx_date() INTO vDate;
    IF NEW.issuedate < vDate THEN
      SET vNewCase = 1;
    END IF;
  END IF;
  SELECT ifnull(sum(ifnull(amount, 0)), 0) INTO vBalance FROM source WHERE id = NEW.id;
  CALL sp_recalculate(NEW.id, vBalance);
  INSERT INTO calculation(id, amount) VALUES (NEW.id, vBalance)
    ON DUPLICATE KEY UPDATE amount = vBalance;
  IF NOT EXISTS(SELECT id FROM archive WHERE originalid = NEW.id LIMIT 1) THEN
    INSERT INTO archive(originalid, amount) SELECT NEW.id, NEW.amount;
    INSERT INTO audit_log(id) VALUE (NEW.id);
  END IF;
END//
DELIMITER ;
CREATE TABLE child(id INT REFERENCES source(id));
CREATE VIEW source_ids AS SELECT id FROM source;
"#;
    fs::write(dir.path().join("migration.sql"), sql).unwrap();
    let stats = extract(&Options::new(dir.path())).unwrap();
    assert_eq!(stats.skipped_sql_data_statements, 0);
    assert_eq!(stats.ignored_sql_statements, 1);
    let graph: Value =
        serde_json::from_slice(&fs::read(dir.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    let source = node(&graph, "table", "app.source");
    let decoy = node(&graph, "table", "other.source");
    for (owner, relation, prefix) in [
        (
            node(&graph, "table", "child"),
            "references",
            "CREATE TABLE child",
        ),
        (
            node(&graph, "view", "source_ids"),
            "reads_from",
            "CREATE VIEW",
        ),
    ] {
        let line = format!(
            "L{}",
            sql.lines()
                .position(|line| line.starts_with(prefix))
                .unwrap()
                + 1
        );
        assert_eq!(owner["source_location"], line);
        assert!(edge(&graph, owner, source, relation));
        assert!(!edge(&graph, owner, decoy, relation));
        let reference = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["source"] == owner["id"] && edge["relation"] == relation)
            .unwrap();
        assert_eq!(reference["source_location"], line);
        assert_eq!(reference["source_file"], "migration.sql");
    }
    let statements: Vec<_> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "sql_statement")
        .collect();
    assert_eq!(statements.len(), 1);
    assert_eq!(statements[0]["source_location"], "L5");
    assert!(
        !graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == statements[0]["id"])
    );
}

#[test]
fn malformed_mysql_local_declarations_and_bodies_preserve_existing_graph() {
    for statement in [
        "DECLARE vBalance DECIMAL(18,2)",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW DECLARE vBalance INT;",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance DEFAULT 0; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance imaginary_type; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance DECIMAL(18,); END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance DECIMAL(18,2) DEFAULT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vOldCase, INT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE 'vBalance' INT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE @vBalance INT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN SET x = 1; DECLARE vBalance INT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN IF NEW.id > 0 THEN DECLARE vBalance INT; END IF; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN IF NEW.id > 0 THEN BEGIN DECLARE vBalance INT; END END IF; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN IF NEW.id > 0 THEN BEGIN DECLARE vBalance INT; END mismatch; END IF; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance, VBALANCE INT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance INT; DECLARE VBALANCE DATE; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE ids CURSOR FOR SELECT id FROM source; DECLARE vBalance INT; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance INT; UPDATE report SET amount = ; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance INT; IF NEW.id > 0 UPDATE report SET amount = 1; END IF; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance INT; IF NEW.id > 0 THEN SET vBalance = 1; END",
        "CREATE TRIGGER broken AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance INT; INSERT INTO audit_log VALUE (NEW.id) nonsense; END",
    ] {
        let dir = TempDir::new().unwrap();
        let mut options = Options::new(dir.path());
        options.sql_dialect = "mysql".into();
        fs::write(
            dir.path().join("known.sql"),
            "CREATE TABLE retained(id INT);",
        )
        .unwrap();
        extract(&options).unwrap();
        let graph_path = dir.path().join("graphify-out/graph.json");
        let previous_graph = fs::read(&graph_path).unwrap();
        fs::write(
            dir.path().join("broken.sql"),
            format!(
                "DELIMITER //\n{statement}//\nDELIMITER ;\nCREATE TABLE after_failure(id INT);"
            ),
        )
        .unwrap();
        let error = extract(&options).unwrap_err();
        assert!(
            format!("{error:#}").contains("broken.sql"),
            "{statement}: {error:#}"
        );
        assert_eq!(fs::read(graph_path).unwrap(), previous_graph, "{statement}");
    }
}
