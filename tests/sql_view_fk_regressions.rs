use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn put(root: &TempDir, file: &str, source: &str) {
    let path = root.path().join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn graph(root: &TempDir) -> Value {
    serde_json::from_slice(&fs::read(root.path().join("graphify-out/graph.json")).unwrap()).unwrap()
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

fn edge<'a>(graph: &'a Value, from: &Value, to: &Value, relation: &str) -> Option<&'a Value> {
    graph["links"].as_array().unwrap().iter().find(|edge| {
        edge["source"] == from["id"] && edge["target"] == to["id"] && edge["relation"] == relation
    })
}

fn targets(graph: &Value, from: &Value, relation: &str) -> Vec<String> {
    graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["source"] == from["id"] && edge["relation"] == relation)
        .map(|edge| edge["target"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn alter_inline_and_constraint_fks_preserve_event_and_projected_table_provenance() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "schema.sql",
        "CREATE TABLE a.users(id INT PRIMARY KEY);\nCREATE TABLE b.users(id INT PRIMARY KEY);\nCREATE TABLE b.orders(id INT);\nALTER TABLE b.orders\n  ADD COLUMN user_id INT\n  REFERENCES users(id);\nALTER TABLE b.orders\n  ADD CONSTRAINT user_fk FOREIGN KEY(user_id) REFERENCES b.users(id);\n-- REFERENCES a.users(id)\n",
    );
    extract(&Options::new(root.path())).unwrap();
    let g = graph(&root);
    let right = node(&g, "schema.sql", "b.users", 2);
    let wrong = node(&g, "schema.sql", "a.users", 1);
    let orders = node(&g, "schema.sql", "b.orders", 3);
    for line in [4, 7] {
        let alter = node(&g, "schema.sql", &format!("ALTER b.orders@{line}"), line);
        assert!(edge(&g, alter, orders, "alters").is_some());
        let fk = edge(&g, alter, right, "references").unwrap();
        assert_eq!(fk["source_location"], format!("L{line}"));
        assert!(edge(&g, alter, wrong, "references").is_none());
    }
    let projected = edge(&g, orders, right, "references").unwrap();
    assert_eq!(projected["source_file"], "schema.sql");
    assert_eq!(projected["source_location"], "L4");
    assert!(edge(&g, orders, wrong, "references").is_none());
}

#[test]
fn views_read_located_tables_and_views_with_join_aliases_and_nested_queries() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "views.sql",
        "CREATE TABLE users(id INT);\nCREATE TABLE audits(id INT);\nCREATE TABLE misleading(id INT);\nCREATE VIEW active_users AS\nSELECT misleading.id FROM\n  users AS misleading\n  JOIN audits AS a ON a.id=misleading.id\nWHERE EXISTS (SELECT 1 FROM users u WHERE u.id=a.id)\nAND 'FROM misleading' <> '';\nCREATE VIEW active_again AS\n SELECT * FROM active_users;\n-- JOIN misleading\n",
    );
    extract(&Options::new(root.path())).unwrap();
    let g = graph(&root);
    let users = node(&g, "views.sql", "users", 1);
    let audits = node(&g, "views.sql", "audits", 2);
    let wrong = node(&g, "views.sql", "misleading", 3);
    let active = node(&g, "views.sql", "active_users", 4);
    let again = node(&g, "views.sql", "active_again", 10);
    assert_eq!(targets(&g, active, "reads_from").len(), 2);
    assert_eq!(
        edge(&g, active, users, "reads_from").unwrap()["source_location"],
        "L6"
    );
    assert_eq!(
        edge(&g, active, audits, "reads_from").unwrap()["source_location"],
        "L7"
    );
    assert!(edge(&g, active, wrong, "reads_from").is_none());
    assert_eq!(
        edge(&g, again, active, "reads_from").unwrap()["source_location"],
        "L11"
    );
    assert!(edge(&g, again, users, "reads_from").is_none());
}

#[test]
fn view_read_token_lines_survive_line_and_nested_block_comments() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "comments.sql",
        "CREATE TABLE users(id INT);\nCREATE TABLE wrong(id INT);\nCREATE VIEW v AS -- FROM wrong\nSELECT * /* JOIN wrong\n /* FROM wrong */\n*/ FROM\nusers;\n",
    );
    let mut options = Options::new(root.path());
    options.sql_dialect = "postgres".into();
    extract(&options).unwrap();
    let g = graph(&root);
    let view = node(&g, "comments.sql", "v", 3);
    assert_eq!(
        edge(&g, view, node(&g, "comments.sql", "users", 1), "reads_from").unwrap()["source_location"],
        "L7"
    );
    assert!(edge(&g, view, node(&g, "comments.sql", "wrong", 2), "reads_from").is_none());
}

#[test]
fn cte_scope_shadowing_and_restoration_do_not_bind_physical_names() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "cte.sql",
        "CREATE TABLE users(id INT);\nCREATE TABLE audits(id INT);\nCREATE TABLE recent(id INT);\nCREATE TABLE inner_only(id INT);\nCREATE VIEW scoped AS\nWITH recent AS (SELECT * FROM users)\nSELECT * FROM recent\nWHERE EXISTS (WITH recent AS (SELECT * FROM audits),\n inner_only AS (SELECT * FROM recent) SELECT * FROM inner_only)\nUNION ALL SELECT * FROM inner_only;\nCREATE VIEW same_name AS WITH users AS (SELECT * FROM users) SELECT * FROM users;\n",
    );
    extract(&Options::new(root.path())).unwrap();
    let g = graph(&root);
    let scoped = node(&g, "cte.sql", "scoped", 5);
    for (name, line) in [("users", 1), ("audits", 2), ("inner_only", 4)] {
        assert!(edge(&g, scoped, node(&g, "cte.sql", name, line), "reads_from").is_some());
    }
    assert_eq!(targets(&g, scoped, "reads_from").len(), 3);
    assert!(edge(&g, scoped, node(&g, "cte.sql", "recent", 3), "reads_from").is_none());
    assert_eq!(
        targets(&g, node(&g, "cte.sql", "same_name", 11), "reads_from"),
        vec![node(&g, "cte.sql", "users", 1)["id"].as_str().unwrap()]
    );
}

#[test]
fn cte_forward_and_self_visibility_follow_the_selected_dialect() {
    for (dialect, recursive, reads_later) in [
        ("mysql", "", true),
        ("mysql", "RECURSIVE ", true),
        ("postgres", "", true),
        ("postgres", "RECURSIVE ", false),
        ("sqlite", "", false),
    ] {
        let root = TempDir::new().unwrap();
        put(
            &root,
            "cte.sql",
            &format!(
                "CREATE TABLE users(id INT);\nCREATE TABLE later(id INT);\nCREATE VIEW v AS WITH {recursive}early AS (SELECT * FROM later), later AS (SELECT * FROM users) SELECT * FROM early;\n"
            ),
        );
        let mut options = Options::new(root.path());
        options.sql_dialect = dialect.into();
        extract(&options).unwrap();
        let g = graph(&root);
        let view = node(&g, "cte.sql", "v", 3);
        assert!(edge(&g, view, node(&g, "cte.sql", "users", 1), "reads_from").is_some());
        assert_eq!(
            edge(&g, view, node(&g, "cte.sql", "later", 2), "reads_from").is_some(),
            reads_later,
            "{dialect} WITH {recursive}"
        );
    }
    for (dialect, recursive, physical) in [
        ("postgres", "", true),
        ("postgres", "RECURSIVE ", false),
        ("sqlite", "", false),
    ] {
        let root = TempDir::new().unwrap();
        put(
            &root,
            "self.sql",
            &format!(
                "CREATE TABLE counter(id INT);\nCREATE VIEW v AS WITH {recursive}counter AS (SELECT 1 AS id UNION ALL SELECT id+1 FROM counter WHERE id<5) SELECT * FROM counter;"
            ),
        );
        let mut options = Options::new(root.path());
        options.sql_dialect = dialect.into();
        extract(&options).unwrap();
        let g = graph(&root);
        assert_eq!(
            edge(
                &g,
                node(&g, "self.sql", "v", 2),
                node(&g, "self.sql", "counter", 1),
                "reads_from"
            )
            .is_some(),
            physical
        );
    }
}

#[test]
fn postgres_quoted_cte_names_and_schema_qualified_tables_are_distinct() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "quoted.sql",
        "CREATE TABLE users(id INT);\nCREATE TABLE \"Recent\"(id INT);\nCREATE TABLE recent(id INT);\nCREATE TABLE app.recent(id INT);\nCREATE VIEW v AS WITH \"Recent\" AS (SELECT * FROM USERS) SELECT * FROM \"Recent\" JOIN recent r ON 1=1 JOIN app.recent p ON 1=1;\n",
    );
    let mut options = Options::new(root.path());
    options.sql_dialect = "postgres".into();
    extract(&options).unwrap();
    let g = graph(&root);
    let view = node(&g, "quoted.sql", "v", 5);
    for (label, line) in [("users", 1), ("recent", 3), ("app.recent", 4)] {
        assert!(edge(&g, view, node(&g, "quoted.sql", label, line), "reads_from").is_some());
    }
    assert!(
        edge(
            &g,
            view,
            node(&g, "quoted.sql", "\"Recent\"", 2),
            "reads_from"
        )
        .is_none()
    );
}

#[test]
fn use_database_keeps_same_named_views_and_their_dependencies_separate() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "use.sql",
        "USE a;\nCREATE TABLE users(id INT);\nCREATE VIEW active AS SELECT * FROM users;\nUSE b;\nCREATE TABLE users(id INT);\nCREATE VIEW active AS SELECT * FROM users;\nCREATE VIEW again AS SELECT * FROM active JOIN a.active other ON 1=1;\n",
    );
    extract(&Options::new(root.path())).unwrap();
    let g = graph(&root);
    let one = node(&g, "use.sql", "active", 3);
    let two = node(&g, "use.sql", "active", 6);
    assert_ne!(one["id"], two["id"]);
    for (view, right, wrong) in [(one, 2, 5), (two, 5, 2)] {
        assert!(edge(&g, view, node(&g, "use.sql", "users", right), "reads_from").is_some());
        assert!(edge(&g, view, node(&g, "use.sql", "users", wrong), "reads_from").is_none());
    }
    let again = node(&g, "use.sql", "again", 7);
    assert!(edge(&g, again, one, "reads_from").is_some());
    assert!(edge(&g, again, two, "reads_from").is_some());
}

#[test]
fn view_resolution_preserves_local_priority_ambiguity_and_explicit_nested_scope() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "one.sql",
        "CREATE TABLE users(id INT);\nCREATE VIEW local_view AS SELECT * FROM users;",
    );
    put(&root, "two.sql", "CREATE TABLE users(id INT);");
    put(
        &root,
        "ambiguous.sql",
        "CREATE VIEW ambiguous_view AS SELECT * FROM users;",
    );
    put(
        &root,
        "migrations/001/base.sql",
        "CREATE TABLE nested(id INT);",
    );
    put(
        &root,
        "migrations/002/view.sql",
        "CREATE VIEW nested_view AS SELECT * FROM nested;",
    );
    let mut options = Options::new(root.path());
    extract(&options).unwrap();
    let g = graph(&root);
    let local = node(&g, "one.sql", "local_view", 2);
    assert!(edge(&g, local, node(&g, "one.sql", "users", 1), "reads_from").is_some());
    assert!(edge(&g, local, node(&g, "two.sql", "users", 1), "reads_from").is_none());
    assert!(
        targets(
            &g,
            node(&g, "ambiguous.sql", "ambiguous_view", 1),
            "reads_from"
        )
        .is_empty()
    );
    assert!(
        targets(
            &g,
            node(&g, "migrations/002/view.sql", "nested_view", 1),
            "reads_from"
        )
        .is_empty()
    );
    options.sql_scope = vec!["migrations/**/*.sql=app".into()];
    extract(&options).unwrap();
    let g = graph(&root);
    assert!(
        edge(
            &g,
            node(&g, "migrations/002/view.sql", "nested_view", 1),
            node(&g, "migrations/001/base.sql", "nested", 1),
            "reads_from"
        )
        .is_some()
    );
    options.exclude = vec!["migrations/001/**".into()];
    extract(&options).unwrap();
    let g = graph(&root);
    assert!(
        targets(
            &g,
            node(&g, "migrations/002/view.sql", "nested_view", 1),
            "reads_from"
        )
        .is_empty()
    );
}

#[test]
fn views_do_not_cross_dump_or_dialect_boundaries_without_explicit_compatible_scope() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "one.sql",
        "-- MySQL dump 10\nCREATE TABLE users(id INT);",
    );
    put(
        &root,
        "two.sql",
        "-- MySQL dump 10\nCREATE VIEW v AS SELECT * FROM users;",
    );
    put(
        &root,
        "postgres/tables.sql",
        "CREATE TABLE only_pg(id INT);",
    );
    put(
        &root,
        "mysql/views.sql",
        "CREATE VIEW wrong_dialect AS SELECT * FROM only_pg;",
    );
    let mut options = Options::new(root.path());
    extract(&options).unwrap();
    let g = graph(&root);
    assert!(targets(&g, node(&g, "two.sql", "v", 2), "reads_from").is_empty());
    options.sql_scope = vec!["**/*.sql=shared".into(), "*.sql=shared".into()];
    extract(&options).unwrap();
    let g = graph(&root);
    assert!(
        edge(
            &g,
            node(&g, "two.sql", "v", 2),
            node(&g, "one.sql", "users", 2),
            "reads_from"
        )
        .is_some()
    );
    assert!(
        targets(
            &g,
            node(&g, "mysql/views.sql", "wrong_dialect", 1),
            "reads_from"
        )
        .is_empty()
    );
}

#[test]
fn views_are_read_targets_but_never_foreign_key_or_alter_targets() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "kind.sql",
        "CREATE TABLE base(id INT);\nCREATE VIEW users AS SELECT * FROM base;\nCREATE TABLE orders(uid INT REFERENCES users(id));\nALTER TABLE orders ADD COLUMN other INT REFERENCES users(id);\nALTER TABLE users ADD COLUMN missing INT;\nCREATE VIEW active AS SELECT * FROM users;\nCREATE TABLE duplicate(id INT);\nCREATE VIEW duplicate AS SELECT * FROM base;\nCREATE TABLE child(uid INT REFERENCES duplicate(id));\nCREATE VIEW ambiguous AS SELECT * FROM duplicate;\n",
    );
    extract(&Options::new(root.path())).unwrap();
    let g = graph(&root);
    let view = node(&g, "kind.sql", "users", 2);
    assert!(targets(&g, node(&g, "kind.sql", "orders", 3), "references").is_empty());
    assert!(targets(&g, node(&g, "kind.sql", "ALTER orders@4", 4), "references").is_empty());
    assert!(targets(&g, node(&g, "kind.sql", "ALTER users@5", 5), "alters").is_empty());
    assert!(edge(&g, node(&g, "kind.sql", "active", 6), view, "reads_from").is_some());
    let table_duplicate = node(&g, "kind.sql", "duplicate", 7);
    let view_duplicate = node(&g, "kind.sql", "duplicate", 8);
    let child = node(&g, "kind.sql", "child", 9);
    assert!(edge(&g, child, table_duplicate, "references").is_some());
    assert!(edge(&g, child, view_duplicate, "references").is_none());
    assert!(targets(&g, node(&g, "kind.sql", "ambiguous", 10), "reads_from").is_empty());
}

#[test]
fn malformed_views_and_alters_preserve_the_previous_complete_graph() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "schema.sql",
        "CREATE TABLE stable(id INT); CREATE VIEW v AS SELECT * FROM stable;",
    );
    extract(&Options::new(root.path())).unwrap();
    let output = root.path().join("graphify-out/graph.json");
    let previous = fs::read(&output).unwrap();
    for invalid in [
        "CREATE TABLE stable(id INT);\nCREATE VIEW v AS SELECT * FROM (",
        "CREATE TABLE stable(id INT);\nALTER TABLE stable ADD COLUMN uid INT REFERENCES;",
        "CREATE TABLE stable(id INT);\nCREATE VIEW v AS WITH c AS (SELECT * FROM stable), c AS (SELECT 1) SELECT * FROM c;",
        "CREATE TABLE stable(id INT);\nCREATE VIEW v AS TABLE stable;",
    ] {
        put(&root, "schema.sql", invalid);
        assert!(
            extract(&Options::new(root.path())).is_err(),
            "accepted {invalid}"
        );
        assert_eq!(fs::read(&output).unwrap(), previous);
    }
}

#[test]
fn streaming_dump_data_does_not_create_relations_or_consume_statement_budget() {
    let root = TempDir::new().unwrap();
    put(
        &root,
        "dump.sql",
        &format!(
            "CREATE TABLE users(id INT);\nCREATE TABLE wrong(id INT);\nINSERT INTO users VALUES ('{} FROM wrong');\nCREATE VIEW v AS\n SELECT * FROM users;\n",
            "x".repeat(2 * 1024 * 1024)
        ),
    );
    let mut options = Options::new(root.path());
    options.max_file_mib = 1;
    let stats = extract(&options).unwrap();
    assert_eq!(stats.skipped_sql_data_statements, 1);
    let g = graph(&root);
    let view = node(&g, "dump.sql", "v", 4);
    assert_eq!(
        edge(&g, view, node(&g, "dump.sql", "users", 1), "reads_from").unwrap()["source_location"],
        "L5"
    );
    assert!(edge(&g, view, node(&g, "dump.sql", "wrong", 2), "reads_from").is_none());
}
