//! MySQL 8.4 expression syntax coverage, checked against the official manual.
//!
//! These tests check extraction, not function evaluation, arity/type checking,
//! privileges, installed plugins, FULLTEXT indexes, or CREATE VIEW server limits.
//! Expressions use the default MySQL SQL mode: no ANSI_QUOTES or PIPES_AS_CONCAT.
//! Function calls intentionally have no space before `(` (IGNORE_SPACE is not
//! required). Values are never executed, so clock/random/connection functions
//! remain deterministic graph tests. No unsupported case is called malformed.
//! Parsing here validates structure, not the complete MySQL grammar. The prior
//! missing-comparison-operand gap is retained as a permanent rejection fixture.

use graphify_extractor::{Options, extract};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

struct Category {
    name: &'static str,
    source: &'static str,
    expressions: &'static [&'static str],
}

// Sources inspected: MySQL 8.4 Reference Manual, chapter 14 and its category
// syntax pages. This is a curated matrix, not a claim of complete MySQL support.
// Index: https://dev.mysql.com/doc/refman/8.4/en/built-in-function-reference.html
// Flow-control syntax: https://dev.mysql.com/doc/refman/8.4/en/flow-control-functions.html
// Operator/cast/regexp category syntax:
// https://dev.mysql.com/doc/refman/8.4/en/arithmetic-functions.html
// https://dev.mysql.com/doc/refman/8.4/en/bit-functions.html
// https://dev.mysql.com/doc/refman/8.4/en/cast-functions.html
// https://dev.mysql.com/doc/refman/8.4/en/regexp.html
const CATEGORIES: &[Category] = &[
    Category {
        name: "numeric",
        source: "https://dev.mysql.com/doc/refman/8.4/en/mathematical-functions.html",
        expressions: &[
            "ABS(decoy.n)",
            "ACOS(decoy.n)",
            "ASIN(decoy.n)",
            "ATAN(decoy.n)",
            "ATAN(decoy.n, 2)",
            "ATAN2(decoy.n, 2)",
            "CEIL(decoy.n)",
            "CEILING(decoy.n)",
            "CONV(decoy.txt, 16, 10)",
            "COS(decoy.n)",
            "COT(decoy.n)",
            "CRC32(decoy.txt)",
            "DEGREES(decoy.n)",
            "EXP(decoy.n)",
            "FLOOR(decoy.n)",
            "LN(decoy.n)",
            "LOG(decoy.n)",
            "LOG(2, decoy.n)",
            "LOG2(decoy.n)",
            "LOG10(decoy.n)",
            "MOD(decoy.i, 3)",
            "PI()",
            "POW(decoy.n, 2)",
            "POWER(decoy.n, 2)",
            "RADIANS(decoy.n)",
            "RAND()",
            "RAND(7)",
            "ROUND(decoy.n)",
            "ROUND(decoy.n, 2)",
            "SIGN(decoy.n)",
            "SIN(decoy.n)",
            "SQRT(decoy.n)",
            "TAN(decoy.n)",
            "TRUNCATE(decoy.n, 2)",
        ],
    },
    Category {
        name: "string",
        source: "https://dev.mysql.com/doc/refman/8.4/en/string-functions.html",
        expressions: &[
            "ASCII(decoy.txt)",
            "BIN(decoy.i)",
            "BIT_LENGTH(decoy.txt)",
            "CHAR(77, 121)",
            "CHAR(77, 121 USING utf8mb4)",
            "CHAR(decoy.i USING 'utf8mb4')",
            "CHAR_LENGTH(decoy.txt)",
            "CHARACTER_LENGTH(decoy.txt)",
            "CONCAT(decoy.txt, ' FROM app.decoy')",
            "CONCAT_WS(',', decoy.txt, 'x')",
            "ELT(1, decoy.txt, 'x')",
            "EXPORT_SET(decoy.i, 'Y', 'N', ',', 8)",
            "FIELD(decoy.txt, 'a', 'b')",
            "FIND_IN_SET(decoy.txt, 'a,b')",
            "FORMAT(decoy.n, 2)",
            "FORMAT(decoy.n, 2, 'de_DE')",
            "FROM_BASE64(decoy.txt)",
            "HEX(decoy.txt)",
            "INSERT(decoy.txt, 1, 1, 'x')",
            "INSTR(decoy.txt, 'x')",
            "LCASE(decoy.txt)",
            "LEFT(decoy.txt, 2)",
            "LENGTH(decoy.txt)",
            "LOAD_FILE(decoy.txt)",
            "LOCATE('x', decoy.txt)",
            "LOCATE('x', decoy.txt, 2)",
            "LOWER(decoy.txt)",
            "LPAD(decoy.txt, 8, '0')",
            "LTRIM(decoy.txt)",
            "MAKE_SET(decoy.i, 'a', 'b')",
            "MID(decoy.txt, 2, 3)",
            "OCT(decoy.i)",
            "OCTET_LENGTH(decoy.txt)",
            "ORD(decoy.txt)",
            "POSITION('x' IN decoy.txt)",
            "QUOTE(decoy.txt)",
            "REPEAT(decoy.txt, 2)",
            "REPLACE(decoy.txt, 'a', 'b')",
            "REVERSE(decoy.txt)",
            "RIGHT(decoy.txt, 2)",
            "RPAD(decoy.txt, 8, '0')",
            "RTRIM(decoy.txt)",
            "SOUNDEX(decoy.txt)",
            "SPACE(decoy.i)",
            "STRCMP(decoy.txt, 'a')",
            "SUBSTR(decoy.txt, 2)",
            "SUBSTRING(decoy.txt FROM 2 FOR 3)",
            "SUBSTRING_INDEX(decoy.txt, '.', 1)",
            "TO_BASE64(decoy.txt)",
            "TRIM(decoy.txt)",
            "TRIM(BOTH 'x' FROM decoy.txt)",
            "TRIM(LEADING 'x' FROM decoy.txt)",
            "TRIM(TRAILING 'x' FROM decoy.txt)",
            "UCASE(decoy.txt)",
            "UNHEX(decoy.txt)",
            "UPPER(decoy.txt)",
        ],
    },
    Category {
        name: "datetime",
        source: "https://dev.mysql.com/doc/refman/8.4/en/date-and-time-functions.html",
        expressions: &[
            "ADDDATE(decoy.d, INTERVAL 1 DAY)",
            "ADDDATE(decoy.d, 1)",
            "ADDTIME(decoy.d, '01:00:00')",
            "CONVERT_TZ(decoy.d, '+00:00', '+01:00')",
            "CURDATE()",
            "CURRENT_DATE",
            "CURRENT_DATE()",
            "CURRENT_TIME",
            "CURRENT_TIME(6)",
            "CURRENT_TIMESTAMP",
            "CURRENT_TIMESTAMP(6)",
            "CURTIME(6)",
            "DATE(decoy.d)",
            "DATE_ADD(decoy.d, INTERVAL 1 MONTH)",
            "DATE_SUB(decoy.d, INTERVAL 1 HOUR)",
            "DATE_SUB(decoy.d, INTERVAL '1:02' HOUR_MINUTE)",
            "DATEDIFF(decoy.d, '2024-01-01')",
            "DATE_FORMAT(decoy.d, '%Y-%m-%d')",
            "DAY(decoy.d)",
            "DAYNAME(decoy.d)",
            "DAYOFMONTH(decoy.d)",
            "DAYOFWEEK(decoy.d)",
            "DAYOFYEAR(decoy.d)",
            "EXTRACT(YEAR FROM decoy.d)",
            "EXTRACT(YEAR_MONTH FROM decoy.d)",
            "FROM_DAYS(decoy.i)",
            "FROM_UNIXTIME(decoy.i)",
            "GET_FORMAT(DATE, 'ISO')",
            "HOUR(decoy.d)",
            "LAST_DAY(decoy.d)",
            "LOCALTIME",
            "LOCALTIMESTAMP(6)",
            "MAKEDATE(2024, decoy.i)",
            "MAKETIME(decoy.i, 1, 2)",
            "MICROSECOND(decoy.d)",
            "MINUTE(decoy.d)",
            "MONTH(decoy.d)",
            "MONTHNAME(decoy.d)",
            "NOW(6)",
            "PERIOD_ADD(decoy.i, 1)",
            "PERIOD_DIFF(decoy.i, 202401)",
            "QUARTER(decoy.d)",
            "SECOND(decoy.d)",
            "SEC_TO_TIME(decoy.i)",
            "STR_TO_DATE(decoy.txt, '%Y-%m-%d')",
            "SUBDATE(decoy.d, 1)",
            "SUBTIME(decoy.d, '01:00:00')",
            "SYSDATE(6)",
            "TIME(decoy.d)",
            "TIMEDIFF(decoy.d, '2024-01-01 00:00:00')",
            "TIMESTAMP(decoy.d)",
            "TIMESTAMPADD(DAY, 1, decoy.d)",
            "TIMESTAMPDIFF(DAY, decoy.d, NOW())",
            "TIME_FORMAT(decoy.d, '%H:%i')",
            "TIME_TO_SEC(decoy.d)",
            "TO_DAYS(decoy.d)",
            "TO_SECONDS(decoy.d)",
            "UNIX_TIMESTAMP(decoy.d)",
            "UTC_DATE()",
            "UTC_TIME(6)",
            "UTC_TIMESTAMP(6)",
            "WEEK(decoy.d, 3)",
            "WEEKDAY(decoy.d)",
            "WEEKOFYEAR(decoy.d)",
            "YEAR(decoy.d)",
            "YEARWEEK(decoy.d, 3)",
            "decoy.d + INTERVAL 1 DAY",
        ],
    },
    Category {
        name: "json",
        source: "https://dev.mysql.com/doc/refman/8.4/en/json-function-reference.html",
        expressions: &[
            "JSON_ARRAY(decoy.i, decoy.txt)",
            "JSON_OBJECT('value', decoy.i)",
            "JSON_QUOTE(decoy.txt)",
            "JSON_CONTAINS(decoy.j, '1', '$.x')",
            "JSON_CONTAINS_PATH(decoy.j, 'one', '$.x')",
            "JSON_EXTRACT(decoy.j, '$.x')",
            "JSON_KEYS(decoy.j)",
            "JSON_OVERLAPS(decoy.j, '[1,2]')",
            "JSON_SEARCH(decoy.j, 'one', 'x')",
            "decoy.j -> '$.x'",
            "decoy.j ->> '$.x'",
            "JSON_ARRAY_APPEND(decoy.j, '$', 1)",
            "JSON_ARRAY_INSERT(decoy.j, '$[0]', 1)",
            "JSON_INSERT(decoy.j, '$.x', 1)",
            "JSON_MERGE_PATCH(decoy.j, '{}')",
            "JSON_MERGE_PRESERVE(decoy.j, '{}')",
            "JSON_REMOVE(decoy.j, '$.x')",
            "JSON_REPLACE(decoy.j, '$.x', 1)",
            "JSON_SET(decoy.j, '$.x', 1)",
            "JSON_UNQUOTE(decoy.j)",
            "JSON_DEPTH(decoy.j)",
            "JSON_LENGTH(decoy.j)",
            "JSON_TYPE(decoy.j)",
            "JSON_VALID(decoy.txt)",
            "JSON_SCHEMA_VALID('{\"type\":\"object\"}', decoy.j)",
            "JSON_SCHEMA_VALIDATION_REPORT('{\"type\":\"object\"}', decoy.j)",
            "JSON_PRETTY(decoy.j)",
            "JSON_STORAGE_FREE(decoy.j)",
            "JSON_STORAGE_SIZE(decoy.j)",
            "1 MEMBER OF(decoy.j)",
            "JSON_VALUE(decoy.j, '$.x' RETURNING UNSIGNED DEFAULT 0 ON EMPTY NULL ON ERROR)",
            "JSON_VALUE(decoy.j, '$.x' RETURNING DECIMAL(8, 2) DEFAULT -1 ON EMPTY DEFAULT +2 ON ERROR)",
            "JSON_VALUE(decoy.j, '$.x' ERROR ON EMPTY ERROR ON ERROR)",
        ],
    },
    Category {
        name: "aggregate",
        source: "https://dev.mysql.com/doc/refman/8.4/en/aggregate-functions.html",
        expressions: &[
            "AVG(decoy.n)",
            "AVG(DISTINCT decoy.n)",
            "BIT_AND(decoy.i)",
            "BIT_OR(decoy.i)",
            "BIT_XOR(decoy.i)",
            "COUNT(*)",
            "COUNT(decoy.i)",
            "COUNT(DISTINCT decoy.i, decoy.txt)",
            "GROUP_CONCAT(decoy.txt)",
            "GROUP_CONCAT(DISTINCT decoy.txt ORDER BY decoy.txt DESC SEPARATOR '|')",
            "JSON_ARRAYAGG(decoy.i)",
            "JSON_OBJECTAGG(decoy.txt, decoy.i)",
            "MAX(decoy.n)",
            "MIN(decoy.n)",
            "STD(decoy.n)",
            "STDDEV(decoy.n)",
            "STDDEV_POP(decoy.n)",
            "STDDEV_SAMP(decoy.n)",
            "SUM(decoy.n)",
            "SUM(DISTINCT decoy.n)",
            "VAR_POP(decoy.n)",
            "VAR_SAMP(decoy.n)",
            "VARIANCE(decoy.n)",
        ],
    },
    Category {
        name: "window",
        source: "https://dev.mysql.com/doc/refman/8.4/en/window-function-descriptions.html",
        expressions: &[
            "CUME_DIST() OVER (ORDER BY decoy.i)",
            "DENSE_RANK() OVER (PARTITION BY decoy.txt ORDER BY decoy.i)",
            "FIRST_VALUE(decoy.n) OVER (ORDER BY decoy.i ROWS UNBOUNDED PRECEDING)",
            "LAG(decoy.n, 1, 0) OVER (ORDER BY decoy.i)",
            "LAST_VALUE(decoy.n) OVER (ORDER BY decoy.i ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
            "LEAD(decoy.n, 1, 0) OVER (ORDER BY decoy.i)",
            "NTH_VALUE(decoy.n, 2) OVER (ORDER BY decoy.i)",
            "NTILE(4) OVER (ORDER BY decoy.i)",
            "PERCENT_RANK() OVER (ORDER BY decoy.i)",
            "RANK() OVER (ORDER BY decoy.i)",
            "ROW_NUMBER() OVER (ORDER BY decoy.i)",
            "SUM(decoy.n) OVER (PARTITION BY decoy.txt ORDER BY decoy.i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)",
            "AVG(decoy.n) OVER (ORDER BY decoy.i RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
        ],
    },
    Category {
        name: "spatial",
        source: "https://dev.mysql.com/doc/refman/8.4/en/spatial-function-reference.html",
        expressions: &[
            "ST_GeomFromText(decoy.txt, 0)",
            "ST_GeomFromWKB(decoy.bytes)",
            "Point(decoy.n, decoy.n)",
            "LineString(Point(0, 0), Point(1, 1))",
            "ST_AsText(decoy.g)",
            "ST_AsBinary(decoy.g)",
            "ST_AsGeoJSON(decoy.g)",
            "ST_GeomFromGeoJSON(decoy.txt)",
            "ST_Dimension(decoy.g)",
            "ST_Envelope(decoy.g)",
            "ST_GeometryType(decoy.g)",
            "ST_IsEmpty(decoy.g)",
            "ST_IsSimple(decoy.g)",
            "ST_SRID(decoy.g)",
            "ST_X(decoy.g)",
            "ST_Y(decoy.g)",
            "ST_Area(decoy.g)",
            "ST_Length(decoy.g)",
            "ST_Centroid(decoy.g)",
            "ST_Contains(decoy.g, Point(0, 0))",
            "ST_Intersects(decoy.g, Point(0, 0))",
            "MBRContains(decoy.g, Point(0, 0))",
            "ST_Distance(decoy.g, Point(0, 0))",
            "ST_Distance_Sphere(decoy.g, Point(0, 0))",
            "ST_Buffer(decoy.g, 1)",
            "ST_Union(decoy.g, Point(0, 0))",
            "ST_Validate(decoy.g)",
        ],
    },
    Category {
        name: "fulltext",
        source: "https://dev.mysql.com/doc/refman/8.4/en/fulltext-search.html",
        expressions: &[
            "MATCH(decoy.txt) AGAINST ('mysql')",
            "MATCH(decoy.txt) AGAINST ('mysql' IN NATURAL LANGUAGE MODE)",
            "MATCH(decoy.txt) AGAINST ('+mysql -decoy' IN BOOLEAN MODE)",
            "MATCH(decoy.txt) AGAINST ('mysql' WITH QUERY EXPANSION)",
            "MATCH(decoy.txt) AGAINST ('mysql' IN NATURAL LANGUAGE MODE WITH QUERY EXPANSION)",
        ],
    },
    Category {
        name: "crypto_compression",
        source: "https://dev.mysql.com/doc/refman/8.4/en/encryption-functions.html",
        expressions: &[
            "AES_ENCRYPT(decoy.txt, 'key')",
            "AES_DECRYPT(decoy.bytes, 'key')",
            "COMPRESS(decoy.txt)",
            "UNCOMPRESS(decoy.bytes)",
            "UNCOMPRESSED_LENGTH(decoy.bytes)",
            "MD5(decoy.txt)",
            "SHA1(decoy.txt)",
            "SHA(decoy.txt)",
            "SHA2(decoy.txt, 256)",
            "RANDOM_BYTES(16)",
            "STATEMENT_DIGEST(decoy.txt)",
            "STATEMENT_DIGEST_TEXT(decoy.txt)",
        ],
    },
    Category {
        name: "information",
        source: "https://dev.mysql.com/doc/refman/8.4/en/information-functions.html",
        expressions: &[
            "BENCHMARK(1, ABS(decoy.n))",
            "CHARSET(decoy.txt)",
            "COERCIBILITY(decoy.txt)",
            "COLLATION(decoy.txt)",
            "CONNECTION_ID()",
            "CURRENT_ROLE()",
            "CURRENT_USER",
            "CURRENT_USER()",
            "DATABASE()",
            "FOUND_ROWS()",
            "LAST_INSERT_ID()",
            "ROW_COUNT()",
            "SCHEMA()",
            "SESSION_USER()",
            "SYSTEM_USER()",
            "USER()",
            "VERSION()",
        ],
    },
    Category {
        name: "comparison_flow",
        source: "https://dev.mysql.com/doc/refman/8.4/en/comparison-operators.html",
        expressions: &[
            "decoy.n <=> NULL",
            "decoy.n BETWEEN 1 AND 3",
            "decoy.n NOT BETWEEN 1 AND 3",
            "COALESCE(decoy.n, 0)",
            "GREATEST(decoy.n, 0)",
            "LEAST(decoy.n, 0)",
            "decoy.i IN (1, 2, 3)",
            "decoy.i IS NULL",
            "decoy.i IS NOT NULL",
            "decoy.i IS TRUE",
            "ISNULL(decoy.i)",
            "INTERVAL(decoy.i, 1, 2, 3)",
            "IF(decoy.i > 0, decoy.txt, 'x')",
            "IFNULL(decoy.txt, 'x')",
            "NULLIF(decoy.i, 0)",
            "CASE WHEN decoy.i > 0 THEN decoy.txt ELSE 'x' END",
            "CASE decoy.i WHEN 1 THEN 'a' ELSE decoy.txt END",
        ],
    },
    Category {
        name: "operators_cast_regexp",
        source: "https://dev.mysql.com/doc/refman/8.4/en/functions.html",
        expressions: &[
            "decoy.i DIV 2",
            "decoy.i % 2",
            "decoy.i MOD 2",
            "~decoy.i",
            "decoy.i << 1",
            "decoy.i >> 1",
            "decoy.n / 2",
            "decoy.i & 3",
            "decoy.i | 3",
            "decoy.i ^ 3",
            "BIT_COUNT(decoy.i)",
            "NOT decoy.i",
            "decoy.i AND 1",
            "decoy.i OR 0",
            "decoy.i XOR 0",
            "decoy.txt LIKE 'a%'",
            "decoy.txt NOT LIKE 'a%'",
            "decoy.txt REGEXP '^a'",
            "decoy.txt RLIKE '^a'",
            "REGEXP_LIKE(decoy.txt, '^a', 'i')",
            "REGEXP_INSTR(decoy.txt, 'a')",
            "REGEXP_REPLACE(decoy.txt, 'a', 'b')",
            "REGEXP_SUBSTR(decoy.txt, 'a')",
            "CAST(decoy.n AS DECIMAL(10, 2))",
            "CAST(decoy.i AS UNSIGNED)",
            "CAST(decoy.i AS SIGNED)",
            "CAST(decoy.txt AS CHAR)",
            "CAST(decoy.txt AS CHAR CHARACTER SET utf8mb4)",
            "CAST(decoy.txt AS CHAR(8) CHARACTER SET 'utf8mb4')",
            "CAST(decoy.txt AS CHAR ASCII)",
            "CAST(decoy.txt AS CHAR UNICODE)",
            "CONVERT(decoy.txt USING utf8mb4)",
            "CONVERT(decoy.n, DECIMAL(10, 2))",
            "decoy.txt COLLATE utf8mb4_bin",
        ],
    },
];

fn graph(root: &TempDir) -> Value {
    serde_json::from_slice(&fs::read(root.path().join("graphify-out/graph.json")).unwrap()).unwrap()
}

fn node<'a>(graph: &'a Value, kind: &str, label: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == kind && node["label"] == label)
        .unwrap_or_else(|| panic!("missing {kind} {label}"))
}

#[test]
fn mysql_documented_function_matrix_preserves_only_real_table_dependencies() {
    let root = TempDir::new().unwrap();
    let mut sql = String::from(
        "CREATE TABLE app.function_source(i INT, n DOUBLE, txt TEXT, d DATETIME, j JSON, g GEOMETRY, bytes BLOB);\n\
         CREATE TABLE other.function_source(i INT);\n\
         CREATE TABLE app.decoy(i INT);\n",
    );
    let mut cases = Vec::new();
    for category in CATEGORIES {
        for expression in category.expressions {
            let label = format!("function_case_{}", cases.len());
            let declaration_line = 4 + cases.len() * 3;
            let statement = format!(
                "CREATE VIEW {label} AS\nSELECT {expression} AS result, 'FROM app.decoy JOIN other.function_source' AS noise\nFROM app.function_source AS decoy;\n"
            );
            assert!(
                category
                    .source
                    .starts_with("https://dev.mysql.com/doc/refman/8.4/en/")
            );
            sql.push_str(&statement);
            cases.push((category.name, *expression, label, declaration_line));
        }
    }
    assert!(cases.len() >= 100);
    fs::write(root.path().join("functions.sql"), sql).unwrap();
    let mut options = Options::new(root.path());
    options.sql_dialect = "mysql".into();
    let stats = extract(&options).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.ignored_sql_statements, 0);
    assert_eq!(stats.skipped_sql_data_statements, 0);
    assert_eq!(stats.unresolved_references, 0);
    let graph = graph(&root);
    let nodes = graph["nodes"].as_array().unwrap();
    assert_eq!(
        nodes.iter().filter(|n| n["type"] == "table").count(),
        3,
        "phantom function/table nodes"
    );
    assert_eq!(
        nodes.iter().filter(|n| n["type"] == "view").count(),
        cases.len()
    );
    let source = node(&graph, "table", "app.function_source");
    let decoy = node(&graph, "table", "app.decoy");
    let other = node(&graph, "table", "other.function_source");
    assert_eq!(source["source_location"], "L1");
    for (category, expression, label, declaration_line) in cases {
        let view = node(&graph, "view", &label);
        assert_eq!(view["source_file"], "functions.sql");
        assert_eq!(view["source_location"], format!("L{declaration_line}"));
        let reads: Vec<_> = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|edge| edge["source"] == view["id"] && edge["relation"] == "reads_from")
            .collect();
        assert_eq!(reads.len(), 1, "{category}: {expression}");
        assert_eq!(reads[0]["target"], source["id"], "{category}: {expression}");
        assert_ne!(reads[0]["target"], decoy["id"]);
        assert_ne!(reads[0]["target"], other["id"]);
        assert_eq!(reads[0]["source_file"], "functions.sql");
        assert_eq!(
            reads[0]["source_location"],
            format!("L{}", declaration_line + 2),
            "{category}: {expression}"
        );
    }
}

fn assert_expressions_fail_without_publishing<'a>(expressions: impl IntoIterator<Item = &'a str>) {
    let root = TempDir::new().unwrap();
    let mut options = Options::new(root.path());
    options.sql_dialect = "mysql".into();
    fs::write(
        root.path().join("known.sql"),
        "CREATE TABLE known_source(i INT, n DOUBLE, txt TEXT, d DATETIME, j JSON);",
    )
    .unwrap();
    extract(&options).unwrap();
    let output = root.path().join("graphify-out/graph.json");
    let previous = fs::read(&output).unwrap();
    for expression in expressions {
        fs::write(root.path().join("unextractable.sql"), format!(
            "CREATE TABLE staged_only(i INT);\nCREATE VIEW candidate AS\nSELECT {expression} AS result\nFROM known_source AS decoy;\nCREATE TABLE after_failure(i INT);\n"
        )).unwrap();
        let error = extract(&options).expect_err(expression);
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("unextractable.sql"),
            "{expression}: {diagnostic}"
        );
        assert!(
            diagnostic.contains("SQL parse near line 2"),
            "{expression}: {diagnostic}"
        );
        assert_eq!(
            fs::read(&output).unwrap(),
            previous,
            "published a partial graph for {expression}"
        );
    }
}

#[test]
fn mysql_malformed_function_expressions_preserve_the_previous_complete_graph() {
    // These are grammar errors, separate from documented-but-unsupported forms.
    // Deliberately do not test server-only errors such as ABS()'s argument count.
    assert_expressions_fail_without_publishing([
        "ABS(decoy.n",
        "CONCAT(decoy.txt,)",
        "SUBSTRING(decoy.txt FROM)",
        "EXTRACT(YEAR decoy.d)",
        "DATE_ADD(decoy.d, INTERVAL)",
        "CAST(decoy.n AS)",
        "CONVERT(decoy.txt USING)",
        "GROUP_CONCAT(decoy.txt ORDER BY)",
        "ROW_NUMBER() OVER (ORDER BY)",
        "JSON_EXTRACT(decoy.j, '$.x'",
        "MATCH(decoy.txt) AGAINST ('x' IN BOOLEAN)",
        "CASE WHEN decoy.i THEN decoy.n",
        "decoy.i <=> )",
        "decoy.i <=> AS",
        "7 DIV AS.x",
        "decoy.i <=> AS.x",
        "decoy.i MOD",
        "decoy.i DIV",
        "decoy.i <<",
        "decoy.i >>",
        "~",
        "decoy.i ~ 1",
        "CHAR(77 USING)",
        "CHAR(77, USING utf8mb4)",
        "CHAR(77 USING (SELECT decoy.i))",
        "DATE_SUB(decoy.d, INTERVAL '1:02' HOUR_BOGUS)",
        "DATE_SUB(decoy.d, INTERVAL '1:02' HOUR_MINUTE extra)",
        "EXTRACT(YEAR_MONTH decoy.d)",
        "EXTRACT(YEAR_BOGUS FROM decoy.d)",
        "INTERVAL(decoy.i,)",
        "INTERVAL(decoy.i)",
        "CAST(decoy.txt AS CHAR CHARACTER SET)",
        "CAST(decoy.txt AS CHAR CHARACTER SET utf8mb4.extra)",
        "CAST(decoy.i AS SIGNED CHARACTER SET utf8mb4)",
        "JSON_VALUE(decoy.j, '$.x' RETURNING)",
        "JSON_VALUE(decoy.j, decoy.txt RETURNING UNSIGNED)",
        "JSON_VALUE(decoy.j, '$.x' DEFAULT ON EMPTY)",
        "JSON_VALUE(decoy.j, '$.x' DEFAULT decoy.i ON EMPTY)",
        "JSON_VALUE(decoy.j, '$.x' DEFAULT NULL ON EMPTY)",
        "JSON_VALUE(decoy.j, '$.x' DEFAULT (SELECT decoy.i) ON EMPTY)",
        "JSON_VALUE(decoy.j, '$.x' NULL ON EMPTY NULL ON EMPTY)",
        "JSON_VALUE(decoy.j, '$.x' NULL ON ERROR ERROR ON EMPTY)",
        "JSON_VALUE(decoy.j, '$.x' RETURNING UNSIGNED DEFAULT 0 ON)",
    ]);
}

#[test]
fn mysql_missing_comparison_operand_fixture_is_rejected_without_publishing() {
    let root = TempDir::new().unwrap();
    let mut options = Options::new(root.path());
    options.sql_dialect = "mysql".into();
    fs::write(
        root.path().join("known.sql"),
        "CREATE TABLE previous_complete(i INT);",
    )
    .unwrap();
    extract(&options).unwrap();
    let output = root.path().join("graphify-out/graph.json");
    let previous = fs::read(&output).unwrap();
    let fixture = include_str!("fixtures/mysql-functions/accepted-invalid.sql");
    fs::write(root.path().join("accepted-invalid.sql"), fixture).unwrap();
    let declaration_line = fixture
        .lines()
        .position(|line| line.starts_with("CREATE VIEW"))
        .unwrap()
        + 1;
    let error = extract(&options).expect_err("missing right operand is invalid MySQL");
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("accepted-invalid.sql"), "{diagnostic}");
    assert!(
        diagnostic.contains(&format!("SQL parse near line {declaration_line}")),
        "{diagnostic}"
    );
    assert_eq!(
        fs::read(&output).unwrap(),
        previous,
        "published an invalid partial graph"
    );
}

#[test]
fn mysql_function_query_variants_visit_nested_sources_without_phantom_relations() {
    // Named windows: https://dev.mysql.com/doc/refman/8.4/en/window-functions-named-windows.html
    // JSON_TABLE: https://dev.mysql.com/doc/refman/8.4/en/json-table-functions.html
    // JSON_VALUE: https://dev.mysql.com/doc/refman/8.4/en/json-search-functions.html#function_json-value
    // Subqueries: https://dev.mysql.com/doc/refman/8.4/en/subqueries.html
    let cases: &[(&str, &[(&str, usize)])] = &[
        (
            "SELECT SUM(s.n) OVER w AS total, ROW_NUMBER() OVER w AS seq\nFROM app.function_source AS s\nWINDOW w AS (PARTITION BY s.txt ORDER BY s.i ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT jt.value\nFROM app.function_source AS s\nJOIN JSON_TABLE(s.j, '$[*]' COLUMNS (ordinality FOR ORDINALITY, value INT PATH '$.x' DEFAULT '0' ON EMPTY NULL ON ERROR)) AS jt ON TRUE",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT jt.x, jt.y\nFROM app.function_source AS s\nJOIN JSON_TABLE(s.j, '$[*]' COLUMNS (x INT PATH '$.x', NESTED PATH '$.items[*]' COLUMNS (y INT PATH '$'))) AS jt ON TRUE",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT COALESCE((SELECT MAX(l.n)\nFROM app.lookup AS l), s.n) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT ABS(derived.n) AS result\nFROM (SELECT IFNULL(s.n, 0) AS n FROM app.function_source AS s) AS derived",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT IF(EXISTS (SELECT 1\nFROM app.lookup AS l WHERE l.n = s.n), s.n, 0) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT JSON_VALUE(s.j, '$.x') AS result\nFROM app.function_source AS s",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT CHAR((SELECT MAX(l.n)\nFROM app.lookup AS l), 121 USING utf8mb4) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT DATE_SUB(s.d, INTERVAL (SELECT MAX(l.n)\nFROM app.lookup AS l) HOUR_MINUTE) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT EXTRACT(YEAR_MONTH FROM (SELECT MAX(l.d)\nFROM app.lookup AS l)) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT INTERVAL((SELECT MAX(l.n)\nFROM app.lookup AS l), s.i, 2, 3) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT s.i MOD (SELECT MAX(l.n)\nFROM app.lookup AS l) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT ~(SELECT MAX(l.n)\nFROM app.lookup AS l) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT s.i << (SELECT MAX(l.n)\nFROM app.lookup AS l) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT s.i >> (SELECT MAX(l.n)\nFROM app.lookup AS l) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT CAST((SELECT MAX(l.n)\nFROM app.lookup AS l) AS CHAR(8) CHARACTER SET utf8mb4) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT JSON_VALUE((SELECT l.j\nFROM app.lookup AS l LIMIT 1), '$.x' RETURNING CHAR(8) CHARACTER SET utf8mb4 DEFAULT '0' ON EMPTY DEFAULT '-1' ON ERROR) AS result\nFROM app.function_source AS s",
            &[("app.lookup", 7), ("app.function_source", 8)],
        ),
        (
            "SELECT s.`AS` <=> `AS` AS result\nFROM app.function_source AS s",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT s.AS AS result\nFROM app.function_source AS s",
            &[("app.function_source", 7)],
        ),
        (
            "SELECT `AS`.i AS result\nFROM app.function_source AS `AS`",
            &[("app.function_source", 7)],
        ),
    ];
    for (query, expected) in cases {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("variants.sql"), format!(
            "CREATE TABLE app.function_source(i INT, n DOUBLE, txt TEXT, d DATETIME, j JSON, `AS` INT);\nCREATE TABLE app.lookup(n DOUBLE, d DATETIME, j JSON);\nCREATE TABLE app.decoy(n DOUBLE);\nCREATE TABLE other.function_source(n DOUBLE);\nCREATE VIEW function_variant AS\n{query};\n"
        )).unwrap();
        let mut options = Options::new(root.path());
        options.sql_dialect = "mysql".into();
        let stats = extract(&options).unwrap_or_else(|error| panic!("{query}: {error:#}"));
        assert_eq!(stats.ignored_sql_statements, 0);
        assert_eq!(stats.unresolved_references, 0, "{query}");
        let graph = graph(&root);
        let view = node(&graph, "view", "function_variant");
        assert_eq!(view["source_location"], "L5");
        let reads: Vec<_> = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|edge| edge["source"] == view["id"] && edge["relation"] == "reads_from")
            .collect();
        assert_eq!(reads.len(), expected.len(), "{query}");
        for (label, line) in *expected {
            let table = node(&graph, "table", label);
            let read = reads
                .iter()
                .find(|edge| edge["target"] == table["id"])
                .unwrap_or_else(|| panic!("missing {label}: {query}"));
            assert_eq!(read["source_file"], "variants.sql");
            assert_eq!(read["source_location"], format!("L{line}"), "{query}");
        }
        for label in ["app.decoy", "other.function_source"] {
            assert!(
                reads
                    .iter()
                    .all(|edge| edge["target"] != node(&graph, "table", label)["id"]),
                "{query}"
            );
        }
        assert_eq!(
            graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|node| node["type"] == "table")
                .count(),
            4
        );
    }
}
