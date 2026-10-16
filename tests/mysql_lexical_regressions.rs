//! MySQL 8.4 lexical contract, independent of server execution.
//! Sources (reviewed 2026-10-16):
//! https://dev.mysql.com/doc/refman/8.4/en/comments.html
//! https://dev.mysql.com/doc/refman/8.4/en/string-literals.html
//! https://dev.mysql.com/doc/refman/8.4/en/identifiers.html
//! https://dev.mysql.com/doc/refman/8.4/en/stored-programs-defining.html
//! https://dev.mysql.com/doc/refman/8.4/en/mysql-commands.html
//! https://dev.mysql.com/doc/refman/8.4/en/sql-mode.html
//! Default MySQL quoting, with statically tracked NO_BACKSLASH_ESCAPES changes.
//! DELIMITER is a mysql client command, not server SQL. Executable comments
//! are extracted without a target-server version filter; no server is executed.
use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn options(root: &TempDir) -> Options {
    let mut o = Options::new(root.path());
    o.sql_dialect = "mysql".into();
    o
}
fn graph(root: &TempDir) -> Value {
    serde_json::from_slice(&fs::read(root.path().join("graphify-out/graph.json")).unwrap()).unwrap()
}
fn located<'a>(g: &'a Value, label: &str, line: usize) -> &'a Value {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| {
            n["label"] == label
                && n["source_file"] == "case.sql"
                && n["source_location"] == format!("L{line}")
        })
        .unwrap_or_else(|| panic!("missing {label} at line {line}: {g}"))
}

#[test]
fn mysql_lexical_variants_preserve_exact_view_targets_and_lines() {
    // Each case changes a lexical boundary, not just a builtin function name.
    let variants = [
        (
            "adjacent_strings",
            "",
            "'FROM ' 'decoy;'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "double_quote_escape",
            "",
            "\"a\"\";FROM decoy\"",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "charset_collation",
            "",
            "_utf8mb4'žltý' COLLATE utf8mb4_bin",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "dash_arithmetic",
            "",
            "id--1",
            "source_table",
            ";",
            "\n",
            true,
        ),
        ("plain", "", "'plain'", "source_table", ";", "\n", true),
        (
            "hash",
            "# FROM decoy;\n",
            "'hash#;text'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "dash",
            "-- FROM decoy;\n",
            "'-- ; not a comment'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "block",
            "/* FROM decoy;\n more */\n",
            "'/*;*/'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "hint",
            "/*+ harmless */\n",
            "'hint'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "doubled_quote",
            "",
            "'it''s; FROM decoy'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "escaped_quote",
            "",
            r"'it\'s; FROM decoy'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "escaped_backslash",
            "",
            r"'a\\;b'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "double_string",
            "",
            "\"FROM decoy;\"",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "national",
            "",
            "N'žltý;文字'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        (
            "charset",
            "",
            "_utf8mb4'žltý;文字'",
            "source_table",
            ";",
            "\n",
            true,
        ),
        ("hex_string", "", "X'4142'", "source_table", ";", "\n", true),
        ("hex_number", "", "0x4142", "source_table", ";", "\n", true),
        ("bit_string", "", "b'0101'", "source_table", ";", "\n", true),
        (
            "quoted_identifier",
            "",
            "'quoted'",
            "`source_table`",
            ";",
            "\n",
            true,
        ),
        (
            "inline_block",
            "",
            "'inline'",
            "/* FROM decoy */ source_table",
            ";",
            "\n",
            true,
        ),
        (
            "crlf",
            "-- comment\n",
            "'crlf'",
            "source_table",
            ";",
            "\r\n",
            true,
        ),
        (
            "no_final_delimiter",
            "",
            "'eof'",
            "source_table",
            ";",
            "\n",
            false,
        ),
        (
            "slash_delimiter",
            "",
            "'//;FROM decoy'",
            "source_table",
            "//",
            "\n",
            true,
        ),
        (
            "dollar_delimiter",
            "",
            "'$$;FROM decoy'",
            "source_table",
            "$$",
            "\n",
            true,
        ),
        (
            "double_semicolon",
            "",
            "';;FROM decoy'",
            "source_table",
            ";;",
            "\n",
            true,
        ),
        (
            "custom_delimiter",
            "",
            "'END_SQL;FROM decoy'",
            "source_table",
            "END_SQL",
            "\n",
            true,
        ),
    ];
    for (name, prefix, expr, table, delimiter, newline, terminated) in variants {
        let root = TempDir::new().unwrap();
        let setup = "CREATE TABLE source_table(id INT);\nCREATE TABLE decoy(id INT);\n";
        let directive = if delimiter == ";" {
            String::new()
        } else {
            format!("DELIMITER {delimiter}\n")
        };
        let before = format!("{setup}{directive}{prefix}");
        let view_line = before.bytes().filter(|b| *b == b'\n').count() + 1;
        let suffix = if terminated { delimiter } else { "" };
        let source = format!(
            "{before}CREATE VIEW chosen AS\nSELECT {expr} AS note, id\nFROM {table}{suffix}\n"
        )
        .replace('\n', newline);
        fs::write(root.path().join("case.sql"), source).unwrap();
        extract(&options(&root)).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let g = graph(&root);
        let view = located(&g, "chosen", view_line);
        let target = located(&g, "source_table", 1);
        let decoy = located(&g, "decoy", 2);
        let reads: Vec<_> = g["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source"] == view["id"] && e["relation"] == "reads_from")
            .collect();
        assert_eq!(reads.len(), 1, "{name}");
        assert_eq!(reads[0]["target"], target["id"], "{name}");
        assert_ne!(reads[0]["target"], decoy["id"], "{name}");
        assert_eq!(reads[0]["source_file"], "case.sql", "{name}");
        assert_eq!(
            reads[0]["source_location"],
            format!("L{}", view_line + 2),
            "{name}"
        );
    }
}

#[test]
fn malformed_mysql_lexical_boundaries_never_replace_previous_graph() {
    for source in [
        "CREATE TABLE broken(id INT); /* unfinished",
        "CREATE VIEW broken AS SELECT 'unfinished;",
        "CREATE TABLE `unfinished(id INT);",
        "/*!80000 CREATE TABLE broken(id INT);",
        "DELIMITER\nCREATE TABLE broken(id INT);",
        "INSERT INTO sample VALUES ('unfinished);",
    ] {
        let root = TempDir::new().unwrap();
        let file = root.path().join("case.sql");
        fs::write(&file, "CREATE TABLE preserved(id INT);\n").unwrap();
        extract(&options(&root)).unwrap();
        let output = root.path().join("graphify-out/graph.json");
        let previous = fs::read(&output).unwrap();
        fs::write(file, source).unwrap();
        let error = extract(&options(&root)).unwrap_err();
        assert!(format!("{error:#}").contains("case.sql"));
        assert_eq!(fs::read(output).unwrap(), previous, "{source}");
    }
}

fn only_edge(
    g: &Value,
    source: &Value,
    target: &Value,
    wrong: &Value,
    relation: &str,
    line: usize,
) {
    let matching: Vec<_> = g["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["source"] == source["id"] && edge["relation"] == relation)
        .collect();
    assert_eq!(matching.len(), 1, "{relation}: {g}");
    assert_eq!(matching[0]["target"], target["id"]);
    assert_ne!(matching[0]["target"], wrong["id"]);
    assert_eq!(matching[0]["source_file"], "case.sql");
    assert_eq!(matching[0]["source_location"], format!("L{line}"));
}

#[test]
fn static_session_modes_preserve_backslash_sensitive_views_and_following_schema() {
    for (index, mode) in [
        "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES'",
        "SET LOCAL `sql_mode` = 'NO_BACKSLASH_ESCAPES'",
        "SET @@SESSION.sql_mode = 'NO_BACKSLASH_ESCAPES'",
        "SET @@sql_mode = 'NO_BACKSLASH_ESCAPES'",
        "/*!80000 SET sql_mode = 'NO_BACKSLASH_ESCAPES' */",
    ]
    .into_iter()
    .enumerate()
    {
        let root = TempDir::new().unwrap();
        let reset = [
            "SET sql_mode = ''",
            "SET SESSION sql_mode = ''",
            "SET LOCAL sql_mode = ''",
            "SET @@SESSION.sql_mode = ''",
            "SET @@LOCAL.sql_mode = ''",
            "SET @@sql_mode = ''",
        ][index];
        let sql = format!(
            r#"CREATE TABLE app.source(id INT PRIMARY KEY);
CREATE TABLE other.source(id INT);
USE app;
{mode};
CREATE VIEW chosen AS
SELECT 'trailing\' AS note, "trailing\" AS double_note,
N'trailing\' AS national_note, _utf8mb4'trailing\' AS charset_note, id
FROM source;
CREATE TABLE child(id INT, FOREIGN KEY(id) REFERENCES source(id));
{reset};
CREATE VIEW reset_chosen AS
SELECT 'it\'s; FROM other.source' AS note, id
FROM source;
"#
        );
        fs::write(root.path().join("case.sql"), sql).unwrap();
        let stats = extract(&options(&root)).unwrap_or_else(|error| panic!("{mode}: {error:#}"));
        assert_eq!(stats.unresolved_references, 0, "{mode}");
        let g = graph(&root);
        let right = located(&g, "app.source", 1);
        let wrong = located(&g, "other.source", 2);
        only_edge(&g, located(&g, "chosen", 5), right, wrong, "reads_from", 8);
        only_edge(&g, located(&g, "child", 9), right, wrong, "references", 9);
        only_edge(
            &g,
            located(&g, "reset_chosen", 11),
            right,
            wrong,
            "reads_from",
            13,
        );
    }
}

#[test]
fn mysqldump_saved_modes_restore_known_lexical_state() {
    let root = TempDir::new().unwrap();
    let sql = r"CREATE TABLE app.source(id INT PRIMARY KEY);
CREATE TABLE other.source(id INT);
USE app;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO,NO_BACKSLASH_ESCAPES' */;
CREATE VIEW chosen AS
SELECT 'trailing\' AS note, id
FROM source;
/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
CREATE TABLE child(id INT, FOREIGN KEY(id) REFERENCES source(id));
CREATE VIEW restored AS
SELECT 'it\'s; FROM other.source' AS note, id
FROM source;
";
    fs::write(root.path().join("case.sql"), sql).unwrap();
    extract(&options(&root)).unwrap();
    let g = graph(&root);
    let right = located(&g, "app.source", 1);
    let wrong = located(&g, "other.source", 2);
    only_edge(&g, located(&g, "chosen", 5), right, wrong, "reads_from", 7);
    only_edge(&g, located(&g, "child", 9), right, wrong, "references", 9);
    only_edge(
        &g,
        located(&g, "restored", 10),
        right,
        wrong,
        "reads_from",
        12,
    );
}

#[test]
fn backticks_use_doubled_quotes_and_literal_backslashes_in_both_modes() {
    for mode in ["", "SET sql_mode='NO_BACKSLASH_ESCAPES';"] {
        let root = TempDir::new().unwrap();
        let sql = format!(
            r"CREATE TABLE `source``table`(id INT);
CREATE TABLE decoy(id INT);
{mode}
CREATE VIEW chosen AS
SELECT id AS `tail\`
FROM `source``table`;
CREATE TABLE child(id INT, FOREIGN KEY(id) REFERENCES `source``table`(id));
"
        );
        fs::write(root.path().join("case.sql"), sql).unwrap();
        extract(&options(&root)).unwrap();
        let g = graph(&root);
        let right = located(&g, "`source``table`", 1);
        let wrong = located(&g, "decoy", 2);
        only_edge(&g, located(&g, "chosen", 4), right, wrong, "reads_from", 6);
        only_edge(&g, located(&g, "child", 7), right, wrong, "references", 7);
    }
}

#[test]
fn mysql_session_modes_are_local_to_each_input_file() {
    for workers in [1, 2] {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("before.sql"),
            "SET sql_mode='NO_BACKSLASH_ESCAPES';\n",
        )
        .unwrap();
        fs::write(
            root.path().join("case.sql"),
            r"CREATE TABLE source(id INT);
CREATE TABLE decoy(id INT);
CREATE VIEW chosen AS SELECT 'it\'s; FROM decoy' AS note, id FROM source;
",
        )
        .unwrap();
        let mut settings = options(&root);
        settings.workers = workers;
        extract(&settings).unwrap();
        let g = graph(&root);
        only_edge(
            &g,
            located(&g, "chosen", 3),
            located(&g, "source", 1),
            located(&g, "decoy", 2),
            "reads_from",
            3,
        );
    }
}

#[test]
fn unknown_or_malformed_mode_changes_preserve_previous_graph() {
    for source in [
        "SET sql_mode = 'ANSI_QUOTES';",
        "SET sql_mode = 'ANSI_QUOTES'",
        "SET sql_mode = 'PIPES_AS_CONCAT';",
        "SET sql_mode = 'ANSI';",
        "SET sql_mode = 'FUTURE_MODE';",
        "SET sql_mode = DEFAULT;",
        "SET sql_mode = @unknown;",
        "SET sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES');",
        "SET sql_mode 'NO_BACKSLASH_ESCAPES';",
        "SET sql_mode = 'NO_BACKSLASH_ESCAPES' trailing;",
        "SET @saved=@@SQL_MODE; SET @saved='ANSI_QUOTES'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SELECT @saved := 'ANSI_QUOTES'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SELECT 'ANSI_QUOTES' INTO @saved; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE, @dummy=(@saved := 'ANSI_QUOTES'); SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; INSERT INTO t VALUES (@saved := 'ANSI_QUOTES'); SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SET @'saved'='ANSI_QUOTES'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SET @\"saved\"='ANSI_QUOTES'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SET @`saved`='ANSI_QUOTES'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SELECT @'saved' := 'ANSI_QUOTES'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; SELECT 'ANSI_QUOTES' INTO @'saved'; SET SQL_MODE=@saved;",
        "SET @saved=@@SQL_MODE; CREATE PROCEDURE change_saved() SET @saved='ANSI_QUOTES'; CALL change_saved(); SET sql_mode=@saved;",
    ] {
        let root = TempDir::new().unwrap();
        let file = root.path().join("case.sql");
        fs::write(&file, "CREATE TABLE preserved(id INT);\n").unwrap();
        extract(&options(&root)).unwrap();
        let output = root.path().join("graphify-out/graph.json");
        let previous = fs::read(&output).unwrap();
        fs::write(&file, format!("-- Mode setting requiring explicit handling\n{source}\nCREATE TABLE after_mode(id INT);\n")).unwrap();
        let error = format!("{:#}", extract(&options(&root)).unwrap_err());
        assert!(
            error.contains("case.sql") && error.contains("sql_mode"),
            "{source}: {error}"
        );
        assert_eq!(fs::read(output).unwrap(), previous, "{source}");
    }
}

#[test]
fn mysql_client_delimiters_follow_argument_and_truncation_semantics() {
    // mysql.cc com_delimiter/get_arg: unquoted values stop at a space, quoted
    // values at their matching quote, and the delimiter buffer retains 15 bytes.
    for (directive, delimiter) in [
        ("DELIMITER too trailing_text", "too"),
        ("DELIMITER abcdefghijklmnopq", "abcdefghijklmno"),
        ("DELIMITER '$ $' trailing_text", "$ $"),
        ("DELIMITER \"//\"", "//"),
        ("DELIMITER `;;`", ";;"),
    ] {
        let root = TempDir::new().unwrap();
        let sql = format!(
            "CREATE TABLE app.source(id INT PRIMARY KEY);\nCREATE TABLE other.source(id INT);\nUSE app;\n{directive}\nCREATE VIEW chosen AS\nSELECT '; FROM other.source' AS note, id\nFROM source{delimiter}\nCREATE TABLE child(id INT, FOREIGN KEY(id) REFERENCES source(id)){delimiter}\nDELIMITER ;\nCREATE VIEW following AS SELECT id FROM source;\n"
        );
        fs::write(root.path().join("case.sql"), sql).unwrap();
        extract(&options(&root)).unwrap_or_else(|error| panic!("{directive}: {error:#}"));
        let g = graph(&root);
        let right = located(&g, "app.source", 1);
        let wrong = located(&g, "other.source", 2);
        only_edge(&g, located(&g, "chosen", 5), right, wrong, "reads_from", 7);
        only_edge(&g, located(&g, "child", 8), right, wrong, "references", 8);
        only_edge(
            &g,
            located(&g, "following", 10),
            right,
            wrong,
            "reads_from",
            10,
        );
    }
}

#[test]
fn malformed_quoted_delimiters_fail_without_replacing_graph() {
    for source in [
        "DELIMITER ''\nCREATE TABLE pending(id INT);\n",
        "DELIMITER 'unfinished\nCREATE TABLE pending(id INT);\n",
        "DELIMITER '\\x'\nCREATE TABLE pending(id INT);\n",
    ] {
        let root = TempDir::new().unwrap();
        let file = root.path().join("case.sql");
        fs::write(&file, "CREATE TABLE preserved(id INT);\n").unwrap();
        extract(&options(&root)).unwrap();
        let output = root.path().join("graphify-out/graph.json");
        let previous = fs::read(&output).unwrap();
        fs::write(file, source).unwrap();
        let error = format!("{:#}", extract(&options(&root)).unwrap_err());
        assert!(
            error.contains("case.sql") && error.contains("DELIMITER"),
            "{error}"
        );
        assert_eq!(fs::read(output).unwrap(), previous, "{source}");
    }
}
