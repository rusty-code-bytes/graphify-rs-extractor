//! Source-grounded MySQL 8.4 compatibility matrix. These are small original
//! examples of documented syntax, not comparisons against another extractor.
//! Parsed routines/triggers have explicit statement coverage, not body edges.
use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

const ROUTINES: &str = "https://dev.mysql.com/doc/refman/8.4/en/create-procedure.html";
const FLOW: &str = "https://dev.mysql.com/doc/refman/8.4/en/flow-control-statements.html";
const DECLARE: &str = "https://dev.mysql.com/doc/refman/8.4/en/declare-local-variable.html";
const CURSOR: &str = "https://dev.mysql.com/doc/refman/8.4/en/cursors.html";
const HANDLER: &str = "https://dev.mysql.com/doc/refman/8.4/en/declare-handler.html";
const TRIGGER: &str = "https://dev.mysql.com/doc/refman/8.4/en/trigger-syntax.html";
const INSERT: &str = "https://dev.mysql.com/doc/refman/8.4/en/insert.html";
const TABLE: &str = "https://dev.mysql.com/doc/refman/8.4/en/create-table.html";
const WITH: &str = "https://dev.mysql.com/doc/refman/8.4/en/with.html";
const COMMENTS: &str = "https://dev.mysql.com/doc/refman/8.4/en/comments.html";
const IDENTIFIERS: &str = "https://dev.mysql.com/doc/refman/8.4/en/identifiers.html";

#[derive(Clone, Copy)]
enum Coverage {
    Statement(&'static str),
    Declaration(&'static str, &'static str),
    Targeted(&'static str, &'static str, &'static str),
    // Top-level data/session statements are intentionally counted omissions.
    Counted { ignored: u64, skipped: u64 },
}

struct Case {
    name: &'static str,
    source: &'static str,
    docs: &'static str,
    coverage: Coverage,
}

fn node<'a>(graph: &'a Value, kind: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["type"] == kind && n["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label}: {graph}"))
}

fn assert_edge(
    graph: &Value,
    from: &Value,
    to: &Value,
    wrong: &Value,
    relation: &str,
    line: usize,
) {
    let links = graph["links"].as_array().unwrap();
    let matching: Vec<_> = links
        .iter()
        .filter(|e| e["source"] == from["id"] && e["relation"] == relation)
        .collect();
    assert_eq!(matching.len(), 1, "{relation}: {graph}");
    assert_eq!(matching[0]["target"], to["id"]);
    assert_ne!(matching[0]["target"], wrong["id"]);
    assert_eq!(matching[0]["source_file"], "matrix.sql");
    assert_eq!(matching[0]["source_location"], format!("L{line}"));
}

fn supported(case: &Case) -> Result<(), String> {
    let root = TempDir::new().unwrap();
    let source = if [
        "CREATE TRIGGER",
        "CREATE PROCEDURE",
        "CREATE FUNCTION",
        "CREATE DEFINER",
    ]
    .iter()
    .any(|prefix| case.source.starts_with(prefix))
    {
        format!("DELIMITER //\n{}//\nDELIMITER ;", case.source)
    } else {
        case.source.to_owned()
    };
    let sql = format!(
        "CREATE TABLE app.source(id INT PRIMARY KEY);\nCREATE TABLE other.source(id INT); CREATE TABLE app.decoy(id INT);\nUSE app;\n{}\nCREATE TABLE probe_child(id INT, FOREIGN KEY(id) REFERENCES source(id));\nCREATE VIEW probe_view AS SELECT id FROM source;\n",
        source
    );
    fs::write(root.path().join("matrix.sql"), &sql).unwrap();
    let mut options = Options::new(root.path());
    options.sql_dialect = "mysql".into();
    let stats = extract(&options).map_err(|e| format!("{} ({}): {e:#}", case.name, case.docs))?;
    let graph: Value =
        serde_json::from_slice(&fs::read(root.path().join("graphify-out/graph.json")).unwrap())
            .unwrap();
    let right = node(&graph, "table", "app.source");
    let wrong = node(&graph, "table", "other.source");
    assert_eq!(right["source_location"], "L1");
    for (kind, label, relation, prefix) in [
        (
            "table",
            "probe_child",
            "references",
            "CREATE TABLE probe_child",
        ),
        ("view", "probe_view", "reads_from", "CREATE VIEW probe_view"),
    ] {
        let line = sql.lines().position(|s| s.starts_with(prefix)).unwrap() + 1;
        let owner = node(&graph, kind, label);
        assert_eq!(owner["source_location"], format!("L{line}"));
        assert_edge(&graph, owner, right, wrong, relation, line);
    }
    match case.coverage {
        Coverage::Statement(prefix) => {
            let nodes: Vec<_> = graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| {
                    n["type"] == "sql_statement" && n["label"].as_str().unwrap().starts_with(prefix)
                })
                .collect();
            assert_eq!(nodes.len(), 1, "{}: {graph}", case.name);
            let line = sql
                .lines()
                .position(|s| s.trim_start().starts_with(prefix))
                .unwrap()
                + 1;
            assert_eq!(nodes[0]["source_location"], format!("L{line}"));
            assert_eq!(nodes[0]["source_file"], "matrix.sql");
            assert_eq!(nodes[0]["extraction_coverage"], "statement_only");
            assert_eq!(stats.sql_statement_only, 1);
            // Body relation extraction is unsupported; never invent edges from
            // a local variable, string, comment, OLD/NEW, cursor, or alias.
            assert!(
                !graph["links"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["source"] == nodes[0]["id"])
            );
            assert!(
                graph["links"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["source"] == node(&graph, "file", "matrix.sql")["id"]
                        && e["target"] == nodes[0]["id"]
                        && e["relation"] == "contains")
            );
            assert_eq!(stats.ignored_sql_statements, 1); // USE only
            assert_eq!(stats.skipped_sql_data_statements, 0);
        }
        Coverage::Declaration(kind, label) => {
            let declaration = node(&graph, kind, label);
            let line = case
                .source
                .lines()
                .position(|line| line.starts_with("CREATE "))
                .unwrap()
                + 4;
            assert_eq!(declaration["source_location"], format!("L{line}"));
            assert_eq!(declaration["source_file"], "matrix.sql");
            if kind == "view" {
                assert_edge(&graph, declaration, right, wrong, "reads_from", line);
                assert!(
                    !graph["links"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["source"] == declaration["id"]
                            && edge["target"] == node(&graph, "table", "app.decoy")["id"]
                            && edge["relation"] == "reads_from")
                );
            }
            assert_eq!(stats.ignored_sql_statements, 1);
            assert_eq!(stats.skipped_sql_data_statements, 0);
        }
        Coverage::Targeted(kind, label, relation) => {
            let declaration = node(&graph, kind, label);
            assert_eq!(declaration["source_location"], "L4");
            assert_edge(&graph, declaration, right, wrong, relation, 4);
            assert_eq!(stats.ignored_sql_statements, 1);
            assert_eq!(stats.skipped_sql_data_statements, 0);
        }
        Coverage::Counted { ignored, skipped } => {
            assert_eq!(stats.ignored_sql_statements, ignored + 1);
            assert_eq!(stats.skipped_sql_data_statements, skipped);
        }
    }
    Ok(())
}

fn trigger(name: &'static str, body: &'static str, docs: &'static str) -> Case {
    Case {
        name,
        source: body,
        docs,
        coverage: Coverage::Statement("CREATE TRIGGER"),
    }
}

#[test]
fn mysql_documented_stored_program_matrix() {
    let cases = [
        trigger(
            "integer local and default",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT DEFAULT 0; SET n = n + 1; END//\nDELIMITER ;",
            DECLARE,
        ),
        trigger(
            "multiple locals and expression default",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE a, b BIGINT DEFAULT (1 + 2); SET a = b; END//\nDELIMITER ;",
            DECLARE,
        ),
        trigger(
            "decimal and date locals",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE amount DECIMAL(18,2); DECLARE day DATE DEFAULT CURRENT_DATE(); SET amount = 1.25; END//\nDELIMITER ;",
            DECLARE,
        ),
        trigger(
            "varchar quoted default",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE note VARCHAR(40) DEFAULT 'FROM other.source; //'; SET note = 'ok'; END//\nDELIMITER ;",
            DECLARE,
        ),
        trigger(
            "cursor declaration",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE c CURSOR FOR SELECT id FROM source; SET @probe = 1; END//\nDELIMITER ;",
            CURSOR,
        ),
        trigger(
            "if else",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT DEFAULT 1; IF n > 0 THEN UPDATE audit_log SET id = id + 1; ELSE DELETE FROM audit_log WHERE id < 0; END IF; END//\nDELIMITER ;",
            FLOW,
        ),
        trigger(
            "if elseif",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT DEFAULT 1; IF n = 0 THEN SET @v = 0; ELSEIF n = 1 THEN SET @v = 1; ELSE SET @v = 2; END IF; END//\nDELIMITER ;",
            FLOW,
        ),
        trigger(
            "nested if",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT DEFAULT 1; IF n > 0 THEN IF n > 1 THEN CALL q(n); END IF; END IF; END//\nDELIMITER ;",
            FLOW,
        ),
        trigger(
            "insert value synonym",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO audit_log(id) VALUE(1); END//\nDELIMITER ;",
            INSERT,
        ),
        trigger(
            "insert set",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO audit_log SET id = 1; END//\nDELIMITER ;",
            INSERT,
        ),
        trigger(
            "insert select",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO audit_log(id) SELECT id + 1 FROM source; END//\nDELIMITER ;",
            INSERT,
        ),
        trigger(
            "duplicate key update",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO audit_log(id) VALUES(1) ON DUPLICATE KEY UPDATE id = VALUES(id); END//\nDELIMITER ;",
            INSERT,
        ),
        trigger(
            "select into local",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT; SELECT COUNT(*) INTO n FROM source; END//\nDELIMITER ;",
            DECLARE,
        ),
        Case {
            name: "trigger after delete old",
            source: "DELIMITER $$\nCREATE TRIGGER t AFTER DELETE ON source FOR EACH ROW BEGIN INSERT INTO audit_log(id) VALUES(OLD.id); END$$\nDELIMITER ;",
            docs: TRIGGER,
            coverage: Coverage::Statement("CREATE TRIGGER"),
        },
        Case {
            name: "trigger update old new",
            source: "DELIMITER |\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN IF OLD.id <> NEW.id THEN INSERT INTO audit_log VALUE(NEW.id); END IF; END|\nDELIMITER ;",
            docs: TRIGGER,
            coverage: Coverage::Statement("CREATE TRIGGER"),
        },
        trigger(
            "simple case statement",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT DEFAULT 1; CASE n WHEN 1 THEN SET @v = 1; ELSE SET @v = 2; END CASE; END//\nDELIMITER ;",
            FLOW,
        ),
        trigger(
            "searched case statement",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN CASE WHEN NEW.id > 0 THEN SET @v = 1; ELSE SET @v = 2; END CASE; END//\nDELIMITER ;",
            FLOW,
        ),
        trigger(
            "boolean and signed locals",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE done BOOLEAN DEFAULT FALSE; DECLARE n BIGINT UNSIGNED DEFAULT 1; SET done = n > 0; END//\nDELIMITER ;",
            DECLARE,
        ),
        trigger(
            "json local",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE payload JSON; SET payload = JSON_OBJECT('id', NEW.id); END//\nDELIMITER ;",
            DECLARE,
        ),
        trigger(
            "quoted local variable",
            "DELIMITER //\nCREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE `select` INT DEFAULT 1; SET @n = `select`; END//\nDELIMITER ;",
            IDENTIFIERS,
        ),
        trigger(
            "before insert compound assignment",
            "DELIMITER //\nCREATE TRIGGER t BEFORE INSERT ON source FOR EACH ROW BEGIN SET NEW.id = ABS(NEW.id); END//\nDELIMITER ;",
            TRIGGER,
        ),
    ];
    let failures: Vec<_> = cases
        .iter()
        .filter_map(|case| supported(case).err())
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn mysql_documented_schema_query_and_counted_data_matrix() {
    use Coverage::{Counted, Declaration, Targeted};
    let cases = [
        Case {
            name: "alter change column",
            source: "ALTER TABLE source CHANGE COLUMN id record_id INT NOT NULL;",
            docs: "https://dev.mysql.com/doc/refman/8.4/en/alter-table.html",
            coverage: Targeted("migration", "ALTER source@4", "alters"),
        },
        Case {
            name: "create unique index",
            source: "CREATE UNIQUE INDEX idx_source ON source(id);",
            docs: "https://dev.mysql.com/doc/refman/8.4/en/create-index.html",
            coverage: Targeted("index", "idx_source@4", "indexes"),
        },
        Case {
            name: "table like",
            source: "CREATE TABLE clone LIKE app.source;",
            docs: TABLE,
            coverage: Declaration("table", "clone"),
        },
        Case {
            name: "create table select",
            source: "CREATE TABLE copied AS SELECT id FROM source;",
            docs: TABLE,
            coverage: Declaration("table", "copied"),
        },
        Case {
            name: "temporary table",
            source: "CREATE TEMPORARY TABLE scratch(id INT) ENGINE=InnoDB;",
            docs: TABLE,
            coverage: Declaration("table", "scratch"),
        },
        Case {
            name: "generated column",
            source: "CREATE TABLE generated_columns(id INT, doubled INT GENERATED ALWAYS AS (id * 2) STORED);",
            docs: TABLE,
            coverage: Declaration("table", "generated_columns"),
        },
        Case {
            name: "check constraint",
            source: "CREATE TABLE checked(id INT, CONSTRAINT positive CHECK (id > 0));",
            docs: TABLE,
            coverage: Declaration("table", "checked"),
        },
        Case {
            name: "table options",
            source: "CREATE TABLE configured(id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;",
            docs: TABLE,
            coverage: Declaration("table", "configured"),
        },
        Case {
            name: "backtick reserved identifier",
            source: "CREATE TABLE `select`(`from` INT, `a``b` VARCHAR(20));",
            docs: IDENTIFIERS,
            coverage: Declaration("table", "`select`"),
        },
        Case {
            name: "view CTE shadows physical decoy",
            source: "CREATE VIEW cte_view AS WITH decoy AS (SELECT id FROM source) SELECT id FROM decoy;",
            docs: WITH,
            coverage: Declaration("view", "cte_view"),
        },
        Case {
            name: "view recursive CTE",
            source: "CREATE VIEW recursive_view AS WITH RECURSIVE ids AS (SELECT id FROM source UNION ALL SELECT id + 1 FROM ids WHERE id < 5) SELECT id FROM ids;",
            docs: WITH,
            coverage: Declaration("view", "recursive_view"),
        },
        Case {
            name: "view derived table",
            source: "CREATE VIEW derived_view AS SELECT id FROM (SELECT id FROM source) AS other;",
            docs: WITH,
            coverage: Declaration("view", "derived_view"),
        },
        Case {
            name: "view quoted schema",
            source: "CREATE VIEW quoted_view AS SELECT `id` FROM `app`.`source`;",
            docs: IDENTIFIERS,
            coverage: Declaration("view", "quoted_view"),
        },
        Case {
            name: "hash comment false reference",
            source: "# FROM other.source\nCREATE VIEW commented_view AS SELECT id FROM source;",
            docs: COMMENTS,
            coverage: Declaration("view", "commented_view"),
        },
        Case {
            name: "insert values counted",
            source: "INSERT INTO source VALUES (1), (2);",
            docs: INSERT,
            coverage: Counted {
                ignored: 0,
                skipped: 1,
            },
        },
        Case {
            name: "insert value counted",
            source: "INSERT INTO source VALUE (1);",
            docs: INSERT,
            coverage: Counted {
                ignored: 0,
                skipped: 1,
            },
        },
        Case {
            name: "insert ignore counted",
            source: "INSERT IGNORE INTO source VALUES (1);",
            docs: INSERT,
            coverage: Counted {
                ignored: 0,
                skipped: 1,
            },
        },
        Case {
            name: "insert priority counted",
            source: "INSERT LOW_PRIORITY INTO source VALUES (1);",
            docs: INSERT,
            coverage: Counted {
                ignored: 0,
                skipped: 1,
            },
        },
        Case {
            name: "insert upsert counted",
            source: "INSERT INTO source VALUES (1) ON DUPLICATE KEY UPDATE id = VALUES(id);",
            docs: INSERT,
            coverage: Counted {
                ignored: 0,
                skipped: 1,
            },
        },
        Case {
            name: "replace counted",
            source: "REPLACE INTO source VALUES (1);",
            docs: "https://dev.mysql.com/doc/refman/8.4/en/replace.html",
            coverage: Counted {
                ignored: 0,
                skipped: 1,
            },
        },
        Case {
            name: "update join counted",
            source: "UPDATE app.source s JOIN other.source o ON s.id=o.id SET s.id=o.id;",
            docs: "https://dev.mysql.com/doc/refman/8.4/en/update.html",
            coverage: Counted {
                ignored: 1,
                skipped: 0,
            },
        },
        Case {
            name: "delete multi table counted",
            source: "DELETE s FROM app.source s JOIN other.source o ON s.id=o.id;",
            docs: "https://dev.mysql.com/doc/refman/8.4/en/delete.html",
            coverage: Counted {
                ignored: 1,
                skipped: 0,
            },
        },
    ];
    let failures: Vec<_> = cases
        .iter()
        .filter_map(|case| supported(case).err())
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Programs whose complete syntax is validated receive explicit statement-only
// coverage. Invisible columns retain the real schema/FK projection.
#[test]
fn mysql_extended_programs_and_invisible_columns_preserve_schema_edges() {
    let cases = [
        (
            "continue handler",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE done INT DEFAULT 0; DECLARE CONTINUE HANDLER FOR NOT FOUND SET done = 1; SELECT COUNT(*) INTO done FROM source; END",
            HANDLER,
        ),
        (
            "exit handler",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION SET @failed = 1; INSERT INTO audit_log VALUES(1); END",
            HANDLER,
        ),
        (
            "named condition",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE missing CONDITION FOR SQLSTATE '45000'; SIGNAL missing; END",
            "https://dev.mysql.com/doc/refman/8.4/en/declare-condition.html",
        ),
        (
            "cursor open fetch close",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT; DECLARE c CURSOR FOR SELECT id FROM source; OPEN c; FETCH c INTO n; CLOSE c; END",
            CURSOR,
        ),
        (
            "while",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN WHILE @n < 3 DO SET @n = @n + 1; END WHILE; END",
            FLOW,
        ),
        (
            "repeat",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN REPEAT SET @n = @n + 1; UNTIL @n > 3 END REPEAT; END",
            FLOW,
        ),
        (
            "labelled loop leave",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN again: LOOP SET @n = @n + 1; IF @n > 3 THEN LEAVE again; END IF; END LOOP again; END",
            FLOW,
        ),
        (
            "basic procedure",
            "CREATE PROCEDURE p() BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "procedure parameters",
            "CREATE PROCEDURE p(IN a INT, OUT b INT, INOUT c INT) BEGIN SET b = a, c = c + a; END",
            ROUTINES,
        ),
        (
            "single statement trigger",
            "CREATE TRIGGER t BEFORE INSERT ON source FOR EACH ROW SET NEW.id = ABS(NEW.id)",
            TRIGGER,
        ),
        (
            "invisible column",
            "CREATE TABLE hidden(id INT, secret INT INVISIBLE REFERENCES source(id));",
            TABLE,
        ),
        (
            "stored function return",
            "CREATE FUNCTION f(n INT) RETURNS INT DETERMINISTIC RETURN n + 1",
            ROUTINES,
        ),
        (
            "procedure characteristics",
            "CREATE PROCEDURE p() SQL SECURITY INVOKER READS SQL DATA BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "definer procedure",
            "CREATE DEFINER = 'app'@'localhost' PROCEDURE p() BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "nested declare block",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN BEGIN DECLARE n INT DEFAULT 1; SET @n = n; END; END",
            DECLARE,
        ),
        (
            "conditional nested declare block",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN IF NEW.id > 0 THEN BEGIN DECLARE vBalance INT; END; END IF; END",
            DECLARE,
        ),
        (
            "labelled iterate and nested scopes",
            "CREATE PROCEDURE p(IN n INT) outer_block: BEGIN DECLARE done INT DEFAULT 0; DECLARE missing CONDITION FOR SQLSTATE '02000'; DECLARE c CURSOR FOR SELECT id FROM source; DECLARE CONTINUE HANDLER FOR missing BEGIN SET done = 1; END; again: LOOP OPEN c; FETCH NEXT FROM c INTO n; IF done THEN LEAVE again; END IF; CLOSE c; BEGIN DECLARE n INT DEFAULT 0; SET n = n + 1; END; ITERATE again; END LOOP again; END outer_block",
            ROUTINES,
        ),
        (
            "function compound return",
            "CREATE FUNCTION f(n INT) RETURNS INT BEGIN IF n > 0 THEN RETURN n; ELSE RETURN 0; END IF; END",
            ROUTINES,
        ),
        (
            "routine if not exists and current definer",
            "CREATE DEFINER = CURRENT_USER() PROCEDURE IF NOT EXISTS p() LANGUAGE SQL NOT DETERMINISTIC MODIFIES SQL DATA COMMENT 'FROM other.source' SELECT id FROM source",
            ROUTINES,
        ),
        (
            "signal simple values",
            "CREATE PROCEDURE p(IN message VARCHAR(40)) BEGIN DECLARE errno INT DEFAULT 1000; SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = message, MYSQL_ERRNO = errno, TABLE_NAME = _utf8mb4'source', COLUMN_NAME = @column_name, SCHEMA_NAME = @@session.sql_mode; END",
            HANDLER,
        ),
        (
            "handler local labels",
            "CREATE PROCEDURE p() outer_block: BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION handler_block: BEGIN LEAVE handler_block; END handler_block; SELECT id FROM source; END outer_block",
            HANDLER,
        ),
    ];
    let failures: Vec<_> = cases
        .iter()
        .filter_map(|(name, source, docs)| {
            let coverage = if *name == "invisible column" {
                Coverage::Targeted("table", "hidden", "references")
            } else {
                Coverage::Statement(if source.starts_with("CREATE DEFINER") {
                    "CREATE DEFINER"
                } else if source.starts_with("CREATE PROCEDURE") {
                    "CREATE PROCEDURE"
                } else if source.starts_with("CREATE FUNCTION") {
                    "CREATE FUNCTION"
                } else {
                    "CREATE TRIGGER"
                })
            };
            supported(&Case {
                name,
                source,
                docs,
                coverage,
            })
            .err()
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn mysql_malformed_programs_fail_atomically() {
    let cases = [
        (
            "handler outer LEAVE label is out of scope",
            "CREATE PROCEDURE p() outer_block: BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION LEAVE outer_block; SELECT id FROM source; END outer_block",
            HANDLER,
        ),
        (
            "handler outer ITERATE label is out of scope",
            "CREATE PROCEDURE p() again: LOOP BEGIN DECLARE CONTINUE HANDLER FOR SQLEXCEPTION ITERATE again; SELECT id FROM source; END; END LOOP again",
            HANDLER,
        ),
        (
            "missing then",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN IF 1 SET @x = 1; END IF; END",
            FLOW,
        ),
        (
            "missing end if",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN IF 1 THEN SET @x = 1; END",
            FLOW,
        ),
        (
            "missing local type",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n; END",
            DECLARE,
        ),
        (
            "missing default expression",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT DEFAULT; END",
            DECLARE,
        ),
        (
            "missing local semicolon",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT SET n = 1; END",
            DECLARE,
        ),
        (
            "declare after executable",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN SET @x = 1; DECLARE n INT; END",
            DECLARE,
        ),
        (
            "duplicate locals",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE n INT; DECLARE N INT; END",
            DECLARE,
        ),
        (
            "variable after cursor",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE c CURSOR FOR SELECT id FROM source; DECLARE n INT; END",
            CURSOR,
        ),
        (
            "cursor missing query",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE c CURSOR FOR; END",
            CURSOR,
        ),
        (
            "missing value row",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO source VALUE; END",
            INSERT,
        ),
        (
            "trailing value comma",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO source VALUE(1,); END",
            INSERT,
        ),
        (
            "upsert missing expression",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO source VALUES(1) ON DUPLICATE KEY UPDATE id =; END",
            INSERT,
        ),
        (
            "body trailing garbage",
            "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN SET @x = 1; END nonsense",
            ROUTINES,
        ),
        (
            "missing trigger each row",
            "CREATE TRIGGER t BEFORE INSERT ON source SET NEW.id = 1",
            TRIGGER,
        ),
        (
            "missing procedure parenthesis",
            "CREATE PROCEDURE p BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
    ];
    rejected(&cases);
}

#[test]
fn mysql_extended_program_grammar_rejects_malformed_input_atomically() {
    let cases = [
        (
            "signal expression instead of simple value",
            "CREATE PROCEDURE p() BEGIN SIGNAL SQLSTATE '45000' SET MYSQL_ERRNO = 1000 + 1; END",
            HANDLER,
        ),
        (
            "signal function instead of simple value",
            "CREATE PROCEDURE p() BEGIN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = CONCAT('a', 'b'); END",
            HANDLER,
        ),
        (
            "handler missing FOR",
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER SQLEXCEPTION SET @x = 1; END",
            HANDLER,
        ),
        (
            "handler missing condition",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR SET @x = 1; END",
            HANDLER,
        ),
        (
            "handler success state",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR SQLSTATE '00000' SET @x = 1; END",
            HANDLER,
        ),
        (
            "handler malformed state",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR SQLSTATE '4500' SET @x = 1; END",
            HANDLER,
        ),
        (
            "handler then variable",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR NOT FOUND SET @x = 1; DECLARE n INT; END",
            HANDLER,
        ),
        (
            "handler then cursor",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR NOT FOUND SET @x = 1; DECLARE c CURSOR FOR SELECT id FROM source; END",
            HANDLER,
        ),
        (
            "handler malformed body",
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION UPDATE source SET id = ; END",
            HANDLER,
        ),
        (
            "condition missing FOR",
            "CREATE PROCEDURE p() BEGIN DECLARE missing CONDITION SQLSTATE '45000'; END",
            HANDLER,
        ),
        (
            "condition zero error",
            "CREATE PROCEDURE p() BEGIN DECLARE missing CONDITION FOR 0; END",
            HANDLER,
        ),
        (
            "condition duplicate",
            "CREATE PROCEDURE p() BEGIN DECLARE missing CONDITION FOR 1051; DECLARE MISSING CONDITION FOR SQLSTATE '45000'; END",
            HANDLER,
        ),
        (
            "condition after cursor",
            "CREATE PROCEDURE p() BEGIN DECLARE c CURSOR FOR SELECT id FROM source; DECLARE missing CONDITION FOR 1051; END",
            HANDLER,
        ),
        (
            "signal undeclared condition",
            "CREATE PROCEDURE p() BEGIN SIGNAL missing; END",
            HANDLER,
        ),
        (
            "signal numeric condition",
            "CREATE PROCEDURE p() BEGIN SIGNAL 1051; END",
            HANDLER,
        ),
        (
            "signal named numeric condition",
            "CREATE PROCEDURE p() BEGIN DECLARE missing CONDITION FOR 1051; SIGNAL missing; END",
            HANDLER,
        ),
        (
            "signal duplicate information",
            "CREATE PROCEDURE p() BEGIN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'a', MESSAGE_TEXT = 'b'; END",
            HANDLER,
        ),
        (
            "open unknown cursor",
            "CREATE PROCEDURE p() BEGIN OPEN c; END",
            CURSOR,
        ),
        (
            "fetch missing INTO",
            "CREATE PROCEDURE p() BEGIN DECLARE n INT; DECLARE c CURSOR FOR SELECT id FROM source; FETCH c n; END",
            CURSOR,
        ),
        (
            "fetch unknown target",
            "CREATE PROCEDURE p() BEGIN DECLARE c CURSOR FOR SELECT id FROM source; FETCH c INTO n; END",
            CURSOR,
        ),
        (
            "fetch NEXT missing FROM",
            "CREATE PROCEDURE p() BEGIN DECLARE n INT; DECLARE c CURSOR FOR SELECT id FROM source; FETCH NEXT c INTO n; END",
            CURSOR,
        ),
        (
            "close unknown cursor",
            "CREATE PROCEDURE p() BEGIN CLOSE c; END",
            CURSOR,
        ),
        (
            "while missing DO",
            "CREATE PROCEDURE p() BEGIN WHILE @n < 3 SET @n = @n + 1; END WHILE; END",
            FLOW,
        ),
        (
            "while mismatched END",
            "CREATE PROCEDURE p() BEGIN WHILE @n < 3 DO SET @n = @n + 1; END LOOP; END",
            FLOW,
        ),
        (
            "repeat missing UNTIL",
            "CREATE PROCEDURE p() BEGIN REPEAT SET @n = @n + 1; END REPEAT; END",
            FLOW,
        ),
        (
            "repeat missing condition",
            "CREATE PROCEDURE p() BEGIN REPEAT SET @n = @n + 1; UNTIL END REPEAT; END",
            FLOW,
        ),
        (
            "repeat missing END REPEAT",
            "CREATE PROCEDURE p() BEGIN REPEAT SET @n = @n + 1; UNTIL @n > 3 END; END",
            FLOW,
        ),
        (
            "loop mismatched label",
            "CREATE PROCEDURE p() BEGIN again: LOOP LEAVE again; END LOOP different; END",
            FLOW,
        ),
        (
            "leave unknown label",
            "CREATE PROCEDURE p() BEGIN LOOP LEAVE missing; END LOOP; END",
            FLOW,
        ),
        (
            "iterate block label",
            "CREATE PROCEDURE p() block_label: BEGIN ITERATE block_label; END block_label",
            FLOW,
        ),
        (
            "nested duplicate labels",
            "CREATE PROCEDURE p() same: BEGIN same: LOOP LEAVE same; END LOOP same; END same",
            FLOW,
        ),
        (
            "unlabelled END label",
            "CREATE PROCEDURE p() BEGIN SET @x = 1; END nonexistent",
            FLOW,
        ),
        (
            "trailing procedure parameter",
            "CREATE PROCEDURE p(IN n INT,) BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "duplicate procedure parameter",
            "CREATE PROCEDURE p(IN n INT, OUT N INT) BEGIN SET n = 1; END",
            ROUTINES,
        ),
        (
            "missing procedure parameter type",
            "CREATE PROCEDURE p(OUT n) BEGIN SET n = 1; END",
            ROUTINES,
        ),
        (
            "custom procedure parameter type",
            "CREATE PROCEDURE p(n imaginary_type) BEGIN SET n = 1; END",
            ROUTINES,
        ),
        (
            "characteristic incomplete data",
            "CREATE PROCEDURE p() READS SQL BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "characteristic invalid security",
            "CREATE PROCEDURE p() SQL SECURITY OWNER BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "characteristic repeated determinism",
            "CREATE PROCEDURE p() DETERMINISTIC NOT DETERMINISTIC BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "definer missing host",
            "CREATE DEFINER='app'@ PROCEDURE p() BEGIN SELECT id FROM source; END",
            ROUTINES,
        ),
        (
            "single trigger missing expression",
            "CREATE TRIGGER t BEFORE INSERT ON source FOR EACH ROW SET NEW.id =",
            TRIGGER,
        ),
        (
            "single trigger trailing tokens",
            "CREATE TRIGGER t BEFORE INSERT ON source FOR EACH ROW SET NEW.id = 1 nonsense",
            TRIGGER,
        ),
        (
            "function missing returns type",
            "CREATE FUNCTION f(n INT) RETURNS DETERMINISTIC RETURN n + 1",
            ROUTINES,
        ),
        (
            "function missing return expression",
            "CREATE FUNCTION f(n INT) RETURNS INT RETURN",
            ROUTINES,
        ),
        (
            "function direction parameter",
            "CREATE FUNCTION f(IN n INT) RETURNS INT RETURN n",
            ROUTINES,
        ),
        (
            "function absent return",
            "CREATE FUNCTION f(n INT) RETURNS INT BEGIN SET n = 1; END",
            ROUTINES,
        ),
        (
            "procedure illegal return",
            "CREATE PROCEDURE p() BEGIN RETURN 1; END",
            ROUTINES,
        ),
        (
            "nested block missing semicolon",
            "CREATE PROCEDURE p() BEGIN BEGIN DECLARE n INT; END END",
            DECLARE,
        ),
        (
            "nested declaration after execution",
            "CREATE PROCEDURE p() BEGIN BEGIN SET @n = 1; DECLARE n INT; END; END",
            DECLARE,
        ),
        (
            "duplicate column visibility",
            "CREATE TABLE hidden(id INT, secret INT INVISIBLE VISIBLE)",
            TABLE,
        ),
        (
            "only invisible columns",
            "CREATE TABLE hidden(id INT INVISIBLE)",
            TABLE,
        ),
        (
            "invisible column malformed suffix",
            "CREATE TABLE hidden(id INT, secret INT INVISIBLE nonsense)",
            TABLE,
        ),
    ];
    rejected(&cases);
}

fn rejected(cases: &[(&str, &str, &str)]) {
    let root = TempDir::new().unwrap();
    let mut options = Options::new(root.path());
    options.sql_dialect = "mysql".into();
    let input = root.path().join("matrix.sql");
    fs::write(
        &input,
        "CREATE TABLE stable(id INT);\nCREATE VIEW stable_view AS SELECT id FROM stable;",
    )
    .unwrap();
    extract(&options).unwrap();
    let graph_path = root.path().join("graphify-out/graph.json");
    let previous = fs::read(&graph_path).unwrap();
    let mut failures = Vec::new();
    for (name, statement, docs) in cases {
        fs::write(&input, format!("CREATE TABLE before_failure(id INT);\nDELIMITER //\n{statement}//\nDELIMITER ;\nCREATE TABLE after_failure(id INT);\n")).unwrap();
        match extract(&options) {
            Ok(_) => {
                failures.push(format!("unexpectedly accepted {name} ({docs})"));
                fs::write(&input, "CREATE TABLE stable(id INT);\nCREATE VIEW stable_view AS SELECT id FROM stable;\n").unwrap();
                extract(&options).unwrap();
            }
            Err(error) => {
                let message = format!("{error:#}");
                assert!(message.contains("matrix.sql"), "{name}: {message}");
                assert!(
                    message.contains("line 3"),
                    "{name}: missing statement provenance: {message}"
                );
                assert_eq!(fs::read(&graph_path).unwrap(), previous, "{name}");
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
