use crate::{Stats, model::Node, store::Store};
use anyhow::{Context, Result, bail};
use sqlparser::{
    ast::{
        AlterTableOperation, ColumnOption, ObjectName, ObjectNamePart, Statement, TableConstraint,
    },
    dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    keywords::Keyword,
    parser::Parser,
    tokenizer::Token,
};
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufReader, Bytes, Read},
    path::Path,
};

// Account-management statements have no table/schema projection. sqlparser's
// generic CREATE USER grammar does not accept MySQL's user@host form. Validate
// the supported complete syntax before counting it as intentionally ignored.
fn mysql_account_control(sql: &str) -> Result<bool> {
    fn word(parser: &mut Parser<'_>, value: &str) -> bool {
        if matches!(parser.peek_token().token, Token::Word(ref w)
            if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(value))
        {
            parser.next_token();
            true
        } else {
            false
        }
    }
    fn expect_word(parser: &mut Parser<'_>, value: &str) -> Result<()> {
        if !word(parser, value) {
            bail!("expected {value} in MySQL account statement");
        }
        Ok(())
    }
    fn account(parser: &mut Parser<'_>) -> Result<()> {
        match parser.next_token().token {
            Token::Word(_) | Token::SingleQuotedString(_) => {}
            _ => bail!("expected MySQL account name"),
        }
        if parser.consume_token(&Token::AtSign) {
            match parser.next_token().token {
                Token::Word(_) | Token::SingleQuotedString(_) => {}
                _ => bail!("expected MySQL account host"),
            }
        }
        Ok(())
    }
    let dialect = MySqlDialect {};
    let mut parser = Parser::new(&dialect).try_with_sql(sql)?;
    if parser.parse_keywords(&[Keyword::CREATE, Keyword::USER]) {
        if word(&mut parser, "IF") {
            expect_word(&mut parser, "NOT")?;
            expect_word(&mut parser, "EXISTS")?;
        }
        loop {
            account(&mut parser)?;
            if word(&mut parser, "IDENTIFIED") {
                if word(&mut parser, "WITH") {
                    parser.parse_identifier()?;
                }
                expect_word(&mut parser, "BY")?;
                if !matches!(parser.next_token().token, Token::SingleQuotedString(_)) {
                    bail!("expected quoted MySQL authentication string");
                }
            }
            if !parser.consume_token(&Token::Comma) {
                break;
            }
        }
    } else if parser.parse_keywords(&[Keyword::GRANT, Keyword::ALL]) {
        word(&mut parser, "PRIVILEGES");
        expect_word(&mut parser, "ON")?;
        for part in 0..2 {
            if !parser.consume_token(&Token::Mul) {
                parser.parse_identifier()?;
            }
            if part == 0 {
                parser.expect_token(&Token::Period)?;
            }
        }
        expect_word(&mut parser, "TO")?;
        loop {
            account(&mut parser)?;
            if !parser.consume_token(&Token::Comma) {
                break;
            }
        }
        if word(&mut parser, "WITH") {
            expect_word(&mut parser, "GRANT")?;
            expect_word(&mut parser, "OPTION")?;
        }
    } else if parser.parse_keywords(&[Keyword::FLUSH, Keyword::PRIVILEGES]) {
        // No arguments: session administration has no schema edges.
    } else {
        return Ok(false);
    }
    if parser.next_token().token != Token::EOF {
        bail!("unsupported MySQL account statement suffix");
    }
    Ok(true)
}

/// Compiled once; no per-reference glob matching or global in-memory table graph.
pub struct Settings<'a> {
    dialect: &'a str,
    scopes: Vec<(globset::GlobMatcher, String)>,
}
impl<'a> Settings<'a> {
    pub fn new(dialect: &'a str, rules: &[String]) -> Result<Self> {
        let mut scopes = Vec::new();
        for rule in rules {
            let (pattern, name) = rule
                .split_once('=')
                .context("sql-scope requires GLOB=NAME")?;
            if pattern.is_empty()
                || pattern.starts_with('/')
                || pattern.split('/').any(|p| p == "..")
                || name.is_empty()
            {
                bail!("invalid root-relative sql-scope: {rule}");
            }
            scopes.push((
                globset::GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()?
                    .compile_matcher(),
                name.to_owned(),
            ));
        }
        Ok(Self { dialect, scopes })
    }
    fn scope(&self, path: &Path, file: &str) -> Result<(String, &'static str, bool)> {
        let mut chosen: Option<&str> = None;
        for (pattern, value) in &self.scopes {
            if pattern.is_match(file) {
                if chosen.is_some_and(|old| old != value) {
                    bail!("conflicting sql-scope rules for {file}");
                }
                chosen = Some(value);
            }
        }
        if let Some(value) = chosen {
            return Ok((
                if value == "@file" {
                    format!("file:{file}")
                } else {
                    format!("named:{value}")
                },
                "explicit",
                true,
            ));
        }
        // Recognized exporter headers are evidence of a self-contained snapshot.
        let mut prefix = Vec::new();
        File::open(path)?.take(8192).read_to_end(&mut prefix)?;
        if String::from_utf8_lossy(&prefix)
            .lines()
            .take_while(|line| {
                let line = line.trim();
                line.is_empty() || line.starts_with("--") || line.starts_with('#')
            })
            .any(|line| {
                let line = line.trim();
                [
                    "-- MySQL dump ",
                    "-- MariaDB dump ",
                    "-- PostgreSQL database dump",
                ]
                .iter()
                .any(|header| line.starts_with(header))
            })
        {
            return Ok((format!("file:{file}"), "dump-header", false));
        }
        let parts: Vec<_> = file.split('/').collect();
        for i in (0..parts.len().saturating_sub(1)).rev() {
            if matches!(
                parts[i].to_ascii_lowercase().as_str(),
                "mysql" | "mariadb" | "pgsql" | "postgres" | "postgresql" | "sqlite" | "sqlite3"
            ) {
                return Ok((
                    format!("tree:{}", parts[..=i].join("/")),
                    "dialect-directory",
                    false,
                ));
            }
        }
        Ok((
            format!("tree:{}", parts[..parts.len() - 1].join("/")),
            "parent-directory",
            false,
        ))
    }
}

// Small bounded lookahead; neither an entire SQL file nor an INSERT row is buffered.
struct Input<R: Read> {
    bytes: Bytes<BufReader<R>>,
    queue: VecDeque<u8>,
    line: usize,
    previous: u8,
}
impl<R: Read> Input<R> {
    fn new(reader: R) -> Self {
        Self {
            bytes: BufReader::new(reader).bytes(),
            queue: VecDeque::new(),
            line: 1,
            previous: b' ',
        }
    }
    fn peek(&mut self, n: usize) -> Result<Option<u8>> {
        while self.queue.len() <= n {
            match self.bytes.next() {
                Some(b) => self.queue.push_back(b?),
                None => return Ok(None),
            }
        }
        Ok(self.queue.get(n).copied())
    }
    fn next(&mut self) -> Result<Option<u8>> {
        let byte = if let Some(b) = self.queue.pop_front() {
            Some(b)
        } else {
            self.bytes.next().transpose()?
        };
        if let Some(b) = byte {
            if b == b'\n' {
                self.line += 1;
            }
            self.previous = b;
        }
        Ok(byte)
    }
    fn starts(&mut self, value: &[u8]) -> Result<bool> {
        for (i, b) in value.iter().enumerate() {
            if self.peek(i)? != Some(*b) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn eat(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.next()?;
        }
        Ok(())
    }
}

fn append(buffer: &mut Vec<u8>, b: u8, skip: bool, limit: usize) -> Result<()> {
    if !skip {
        if buffer.len() >= limit {
            bail!("SQL statement exceeds configured size limit");
        }
        buffer.push(b);
    }
    Ok(())
}

fn copy_data<R: Read>(input: &mut Input<R>) -> Result<()> {
    // Raw COPY text ends at a line containing only \.; no allocation per data row.
    let mut prefix = [0u8; 3];
    let mut len = 0usize;
    while let Some(b) = input.next()? {
        if b == b'\n' {
            if (len == 2 && &prefix[..2] == b"\\.") || (len == 3 && &prefix == b"\\.\r") {
                return Ok(());
            }
            len = 0;
        } else {
            if len < 3 {
                prefix[len] = b;
            }
            len = (len + 1).min(4);
        }
    }
    if (len == 2 && &prefix[..2] == b"\\.") || (len == 3 && &prefix == b"\\.\r") {
        return Ok(());
    }
    bail!("unterminated COPY FROM STDIN data");
}

/// Split lexical statements, respecting strings/comments/dollar quotes and MySQL delimiters.
/// The callback sees empty bytes for intentionally skipped INSERT/REPLACE statements.
fn statements<R: Read>(
    reader: R,
    dialect: &str,
    ansi_comments: bool,
    limit: usize,
    mut emit: impl FnMut(&[u8], usize, bool) -> Result<()>,
) -> Result<()> {
    let mut input = Input::new(reader);
    let mut buffer = Vec::new();
    let mut skip = false;
    let mut first_word_done = false;
    let mut line = 1;
    let mut delimiter = b";".to_vec();
    let mut executable_comment = false;
    while let Some(b) = input.peek(0)? {
        if buffer.is_empty() && !skip && b.is_ascii_whitespace() {
            input.next()?;
            continue;
        }
        if buffer.is_empty() && !skip {
            line = input.line;
        }
        if executable_comment && input.starts(b"*/")? {
            input.eat(2)?;
            executable_comment = false;
            continue;
        }
        if input.starts(&delimiter)? {
            input.eat(delimiter.len())?;
            let text = String::from_utf8_lossy(&buffer).to_ascii_uppercase();
            let words: Vec<&str> = text.split_whitespace().collect();
            let is_copy = dialect == "postgres"
                && words.first() == Some(&"COPY")
                && words.windows(2).any(|w| w == ["FROM", "STDIN"]);
            if is_copy && words.iter().any(|w| w.contains("BINARY")) {
                bail!("binary COPY input is not supported; export a text SQL dump");
            }
            if skip || is_copy {
                emit(&[], line, true)?;
            } else if !buffer.iter().all(u8::is_ascii_whitespace) {
                emit(&buffer, line, false)?;
            }
            buffer.clear();
            skip = false;
            first_word_done = false;
            if is_copy {
                copy_data(&mut input)?;
            }
            continue;
        }
        if input.starts(b"--")?
            && (ansi_comments
                || dialect != "mysql"
                || input.peek(2)?.is_none_or(|c| c.is_ascii_whitespace()))
            || (dialect == "mysql" && b == b'#')
        {
            while let Some(c) = input.next()? {
                if c == b'\n' {
                    break;
                }
            }
            if !buffer.is_empty() {
                append(&mut buffer, b' ', skip, limit)?;
            }
            continue;
        }
        if input.starts(b"/*")? {
            input.eat(2)?;
            if dialect == "mysql" && input.peek(0)? == Some(b'!') {
                if executable_comment {
                    bail!("nested executable SQL comment");
                }
                input.next()?;
                while input.peek(0)?.is_some_and(|c| c.is_ascii_digit()) {
                    input.next()?;
                }
                executable_comment = true;
                continue;
            }
            let mut nesting = 1usize;
            while nesting > 0 {
                if input.starts(b"*/")? {
                    input.eat(2)?;
                    nesting -= 1;
                } else if dialect == "postgres" && input.starts(b"/*")? {
                    input.eat(2)?;
                    nesting += 1;
                } else if input.next()?.is_none() {
                    bail!("unterminated SQL comment near line {line}");
                }
            }
            if !buffer.is_empty() {
                append(&mut buffer, b' ', skip, limit)?;
            }
            continue;
        }
        // Classify at the end of the first keyword, before retaining any INSERT payload.
        if !first_word_done && !b.is_ascii_alphabetic() && !buffer.is_empty() {
            let keyword = String::from_utf8_lossy(&buffer)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if keyword == "DELIMITER" {
                if dialect != "mysql" {
                    bail!("DELIMITER requires --sql-dialect mysql");
                }
                let mut value = Vec::new();
                while let Some(c) = input.next()? {
                    if c == b'\n' {
                        break;
                    }
                    if value.len() >= 64 {
                        bail!("invalid SQL DELIMITER");
                    }
                    value.push(c);
                }
                let value = std::str::from_utf8(&value)?.trim().as_bytes();
                if value.is_empty() || value.len() > 16 || value.iter().any(u8::is_ascii_whitespace)
                {
                    bail!("invalid SQL DELIMITER");
                }
                delimiter = value.to_vec();
                buffer.clear();
                continue;
            }
            if matches!(keyword.as_str(), "INSERT" | "REPLACE") {
                skip = true;
                buffer.clear();
            }
            first_word_done = true;
        }
        if matches!(b, b'\'' | b'"' | b'`') || (b == b'[' && dialect == "sqlite") {
            let quote = if b == b'[' { b']' } else { b };
            let backslash = dialect == "mysql"
                || (dialect == "postgres" && b == b'\'' && matches!(input.previous, b'e' | b'E'));
            input.next()?;
            append(&mut buffer, b, skip, limit)?;
            loop {
                let c = input
                    .next()?
                    .context("unterminated SQL quoted string/identifier")?;
                append(&mut buffer, c, skip, limit)?;
                if backslash && c == b'\\' {
                    let escaped = input.next()?.context("unterminated SQL escape")?;
                    append(&mut buffer, escaped, skip, limit)?;
                } else if c == quote {
                    if input.peek(0)? == Some(quote) {
                        input.next()?;
                        append(&mut buffer, quote, skip, limit)?;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }
        if dialect == "postgres" && b == b'$' {
            let mut tag = vec![b'$'];
            let mut i = 1;
            while i <= 128 {
                match input.peek(i)? {
                    Some(b'$') => {
                        tag.push(b'$');
                        break;
                    }
                    Some(c) if c.is_ascii_alphanumeric() || c == b'_' => tag.push(c),
                    _ => {
                        tag.clear();
                        break;
                    }
                }
                i += 1;
            }
            if i > 128 {
                bail!("SQL dollar-quote tag exceeds 128 bytes");
            }
            if !tag.is_empty() {
                input.eat(tag.len())?;
                for &c in &tag {
                    append(&mut buffer, c, skip, limit)?;
                }
                loop {
                    if input.starts(&tag)? {
                        input.eat(tag.len())?;
                        for &c in &tag {
                            append(&mut buffer, c, skip, limit)?;
                        }
                        break;
                    }
                    let c = input.next()?.context("unterminated SQL dollar quote")?;
                    append(&mut buffer, c, skip, limit)?;
                }
                continue;
            }
        }
        input.next()?;
        append(&mut buffer, b, skip, limit)?;
    }
    if executable_comment {
        bail!("unterminated executable SQL comment");
    }
    if skip {
        emit(&[], line, true)?;
    } else if !buffer.iter().all(u8::is_ascii_whitespace) {
        emit(&buffer, line, false)?;
    }
    Ok(())
}

fn object_key(name: &ObjectName, dialect: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in &name.0 {
        let ObjectNamePart::Identifier(ident) = part else {
            bail!("dynamic SQL object identifier");
        };
        parts.push(
            if (dialect == "postgres" && ident.quote_style.is_none()) || dialect == "sqlite" {
                ident.value.to_ascii_lowercase()
            } else {
                ident.value.clone()
            },
        );
    }
    Ok(serde_json::to_string(&parts)?)
}

pub fn extract(
    path: &Path,
    file: &str,
    file_id: &str,
    store: &Store,
    settings: &Settings<'_>,
    limit: usize,
    stats: &mut Stats,
) -> Result<()> {
    // Mixed repositories often keep vendor-specific scripts in named directories.
    // Explicit dialects override this policy; files without a hint retain MySQL semantics.
    let dialect_name = settings.dialect;
    let ansi_comments = dialect_name == "auto";
    let dialect_name = if dialect_name == "auto" {
        file.split('/')
            .rev()
            .skip(1)
            .find_map(|part| match part.to_ascii_lowercase().as_str() {
                "pgsql" | "postgres" | "postgresql" => Some("postgres"),
                "mysql" | "mariadb" => Some("mysql"),
                "sqlite" | "sqlite3" => Some("sqlite"),
                _ => None,
            })
            .unwrap_or("mysql")
    } else {
        dialect_name
    };
    let dialect: Box<dyn Dialect> = match dialect_name {
        "mysql" => Box::new(MySqlDialect {}),
        "postgres" => Box::new(PostgreSqlDialect {}),
        "sqlite" => Box::new(SQLiteDialect {}),
        "generic" => Box::new(GenericDialect {}),
        _ => bail!("unsupported SQL dialect {dialect_name}"),
    };
    let (scope, reason, explicit_scope) = settings.scope(path, file)?;
    let role = if file.split('/').rev().skip(1).any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "migration" | "migrations"
        )
    }) {
        "migration"
    } else {
        "schema"
    };
    store.sql_file(file, file_id, dialect_name, &scope, reason, role)?;
    let mut schema = None::<String>;
    statements(
        File::open(path)?,
        dialect_name,
        ansi_comments,
        limit,
        |bytes, line, skipped| {
            if skipped {
                stats.skipped_sql_data_statements += 1;
                return Ok(());
            }
            let sql = std::str::from_utf8(bytes).context("SQL schema statements must be UTF-8")?;
            let keyword = sql
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if dialect_name == "mysql" && mysql_account_control(sql)? {
                stats.ignored_sql_statements += 1;
                return Ok(());
            }
            // Session/data/transaction statements do not define schema. Count their omission.
            if matches!(
                keyword.as_str(),
                "SET"
                    | "LOCK"
                    | "UNLOCK"
                    | "BEGIN"
                    | "COMMIT"
                    | "ROLLBACK"
                    | "START"
                    | "SELECT"
                    | "UPDATE"
                    | "DELETE"
                    | "TRUNCATE"
            ) {
                if sql.to_ascii_lowercase().contains("search_path")
                    || sql.to_ascii_lowercase().contains("sql_mode")
                        && sql.to_ascii_uppercase().contains("NO_BACKSLASH_ESCAPES")
                {
                    bail!(
                        "session setting changes SQL resolution/lexing near line {line}; normalize the dump first"
                    );
                }
                stats.ignored_sql_statements += 1;
                return Ok(());
            }
            if keyword == "USE" {
                let parsed = Parser::parse_sql(dialect.as_ref(), sql)
                    .with_context(|| format!("SQL parse near line {line}"))?;
                if parsed.len() != 1 {
                    bail!("invalid USE statement");
                }
                // USE is restricted to one unambiguous database identifier.
                let value = sql.trim()[3..].trim().trim_matches('`');
                if value.is_empty() || value.contains([' ', '\t', '\n', '"', '\'']) {
                    bail!("unsupported USE database identifier");
                }
                schema = Some(value.into());
                stats.ignored_sql_statements += 1;
                return Ok(());
            }
            // sqlparser 0.59 does not cover these valid vendor statements. Parse their
            // complete token structure rather than dropping arbitrary failed statements.
            let mut extra = Parser::new(dialect.as_ref()).try_with_sql(sql)?;
            if dialect_name == "postgres"
                && extra.parse_keywords(&[Keyword::CREATE, Keyword::LANGUAGE])
            {
                let name = extra.parse_identifier()?;
                if extra.next_token().token != Token::EOF {
                    bail!("unsupported CREATE LANGUAGE suffix near line {line}");
                }
                let node = Node::new(file, "language", &name.to_string(), "sql", line);
                store.node(&node)?;
                store.edge(file_id, &node.id, "defines", file, line)?;
                return Ok(());
            }
            let mut extra = Parser::new(dialect.as_ref()).try_with_sql(sql)?;
            if dialect_name == "mysql" && extra.parse_keywords(&[Keyword::ALTER, Keyword::TABLE]) {
                let name = extra.parse_object_name(false)?;
                let action = if extra.parse_keywords(&[Keyword::DISABLE, Keyword::KEYS]) {
                    Some("DISABLE")
                } else if extra.parse_keywords(&[Keyword::ENABLE, Keyword::KEYS]) {
                    Some("ENABLE")
                } else {
                    None
                };
                if let Some(action) = action {
                    // Dump control commands isolate even headerless snapshots. Explicit
                    // contexts can group a deliberately split dump across several files.
                    if !explicit_scope {
                        store.sql_file(
                            file,
                            file_id,
                            dialect_name,
                            &format!("file:{file}"),
                            "dump-keys",
                            role,
                        )?;
                    }
                    if extra.next_token().token != Token::EOF {
                        bail!("unexpected ALTER KEYS suffix near line {line}");
                    }
                    let node = Node::new(
                        file,
                        "migration",
                        &format!("ALTER {name} {action} KEYS@{line}"),
                        "sql",
                        line,
                    );
                    store.node(&node)?;
                    store.edge(file_id, &node.id, "contains", file, line)?;
                    let mut parts: Vec<String> =
                        serde_json::from_str(&object_key(&name, dialect_name)?)?;
                    if parts.len() == 1
                        && let Some(schema) = &schema
                    {
                        parts.insert(0, schema.clone());
                    }
                    store.reference(
                        &node.id,
                        "sql-table",
                        &serde_json::to_string(&parts)?,
                        None,
                        "alters",
                        file,
                        line,
                    )?;
                    return Ok(());
                }
            }
            let statements = Parser::parse_sql(dialect.as_ref(), sql)
                .with_context(|| format!("SQL parse near line {line}"))?;
            let key = |name: &ObjectName| -> Result<String> {
                let raw = object_key(name, dialect_name)?;
                if name.0.len() == 1
                    && let Some(schema) = &schema
                {
                    let mut parts: Vec<String> = serde_json::from_str(&raw)?;
                    parts.insert(0, schema.clone());
                    return Ok(serde_json::to_string(&parts)?);
                }
                Ok(raw)
            };
            let foreign_key = |owner: &str, owner_table: &ObjectName, target: &ObjectName| {
                let target_key =
                    if dialect_name == "mysql" && target.0.len() == 1 && owner_table.0.len() > 1 {
                        let mut qualified = owner_table.0.clone();
                        qualified.pop();
                        qualified.extend(target.0.clone());
                        object_key(&ObjectName(qualified), dialect_name)?
                    } else {
                        key(target)?
                    };
                store.reference(
                    owner,
                    "sql-table",
                    &target_key,
                    None,
                    "references",
                    file,
                    line,
                )
            };
            for statement in statements {
                match statement {
                    Statement::CreateTable(table) => {
                        let mut node = Node::new(file, "table", &key(&table.name)?, "sql", line);
                        node.label = table.name.to_string();
                        store.node(&node)?;
                        store.symbol("sql-table", &key(&table.name)?, &node.id)?;
                        store.sql_definition(file, &key(&table.name)?, &node.id)?;
                        store.edge(file_id, &node.id, "defines", file, line)?;
                        for column in table.columns {
                            let mut child = Node::new(
                                file,
                                "column",
                                &format!("{}::{}", key(&table.name)?, column.name),
                                "sql",
                                line,
                            );
                            child.label = format!("{}.{}", table.name, column.name);
                            store.node(&child)?;
                            store.edge(&node.id, &child.id, "contains", file, line)?;
                            for option in column.options {
                                if let ColumnOption::ForeignKey { foreign_table, .. } =
                                    option.option
                                {
                                    foreign_key(&node.id, &table.name, &foreign_table)?;
                                }
                            }
                        }
                        for constraint in table.constraints {
                            if let TableConstraint::ForeignKey { foreign_table, .. } = constraint {
                                foreign_key(&node.id, &table.name, &foreign_table)?;
                            }
                        }
                    }
                    Statement::CreateView { name, .. } => {
                        let node = Node::new(file, "view", &name.to_string(), "sql", line);
                        store.node(&node)?;
                        store.edge(file_id, &node.id, "defines", file, line)?;
                    }
                    Statement::AlterTable {
                        name, operations, ..
                    } => {
                        let node = Node::new(
                            file,
                            "migration",
                            &format!("ALTER {name}@{line}"),
                            "sql",
                            line,
                        );
                        store.node(&node)?;
                        store.edge(file_id, &node.id, "contains", file, line)?;
                        store.reference(
                            &node.id,
                            "sql-table",
                            &key(&name)?,
                            None,
                            "alters",
                            file,
                            line,
                        )?;
                        for operation in operations {
                            if let AlterTableOperation::AddConstraint {
                                constraint: TableConstraint::ForeignKey { foreign_table, .. },
                                ..
                            } = operation
                            {
                                foreign_key(&node.id, &name, &foreign_table)?;
                            }
                        }
                    }
                    Statement::CreateIndex(index) => {
                        let node = Node::new(
                            file,
                            "index",
                            &format!(
                                "{}@{line}",
                                index
                                    .name
                                    .map(|n| n.to_string())
                                    .unwrap_or_else(|| "index".into())
                            ),
                            "sql",
                            line,
                        );
                        store.node(&node)?;
                        store.edge(file_id, &node.id, "defines", file, line)?;
                        store.reference(
                            &node.id,
                            "sql-table",
                            &key(&index.table_name)?,
                            None,
                            "indexes",
                            file,
                            line,
                        )?;
                    }
                    Statement::Drop { .. } => {
                        // Record the declaration event; do not pretend migrations were executed.
                        let node = Node::new(
                            file,
                            "migration",
                            &format!("{statement}@{line}"),
                            "sql",
                            line,
                        );
                        store.node(&node)?;
                        store.edge(file_id, &node.id, "contains", file, line)?;
                    }
                    other => {
                        // Preserve unsupported-but-parsed statements as explicit statement nodes.
                        // No silently invented relationships or false claim of full SQL semantics.
                        let node = Node::new(
                            file,
                            "sql_statement",
                            &format!(
                                "{}@{line}",
                                other.to_string().chars().take(160).collect::<String>()
                            ),
                            "sql",
                            line,
                        );
                        store.node(&node)?;
                        store.edge(file_id, &node.id, "contains", file, line)?;
                    }
                }
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auto_comments_and_strict_mysql_arithmetic_are_distinct() {
        let sql = b"SELECT 4--1;";
        let mut strict = Vec::new();
        statements(sql.as_slice(), "mysql", false, 128, |s, _, _| {
            strict.push(s.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(strict, vec![b"SELECT 4--1".to_vec()]);
        let mut script = Vec::new();
        statements(
            b"CREATE TABLE t(a INT,\n--hidden INT,\nb INT);".as_slice(),
            "mysql",
            true,
            128,
            |s, _, _| {
                script.push(s.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert!(!String::from_utf8_lossy(&script[0]).contains("hidden"));
    }
    #[test]
    fn lexer_preserves_quoted_semicolons_and_skips_large_insert() {
        let sql = format!(
            "-- fake;\nCREATE TABLE t(x text DEFAULT ';'); INSERT/* comment */INTO t VALUES ('{};'); CREATE TABLE z(x int);",
            "x".repeat(100_000)
        );
        let mut got = Vec::new();
        statements(sql.as_bytes(), "mysql", false, 128, |s, _, skip| {
            got.push((String::from_utf8_lossy(s).into_owned(), skip));
            Ok(())
        })
        .unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[1].1);
        assert!(got[2].0.contains("CREATE TABLE z"));
    }
    #[test]
    fn dollar_quotes_copy_and_delimiter() {
        let mut count = 0;
        statements(
            b"COPY t FROM STDIN;\nx;not sql\n\\.\nCREATE TABLE t(x text);".as_slice(),
            "postgres",
            false,
            1024,
            |_, _, _| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 2);
        let mut count = 0;
        statements(b"DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT ';'; END$$\nDELIMITER ;\nCREATE TABLE t(x int);".as_slice(),"mysql",false,1024,|_,_,_|{ count+=1; Ok(()) }).unwrap();
        assert_eq!(count, 2);
        let mut count = 0;
        statements(
            b"SELECT $tag$a;b$tag$; SELECT 2;".as_slice(),
            "postgres",
            false,
            1024,
            |_, _, _| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 2);
    }
    #[test]
    fn malformed_sql_is_not_silently_accepted() {
        for sql in ["INSERT INTO t VALUES ('oops", "/* unfinished", "SELECT $$x"] {
            assert!(statements(sql.as_bytes(), "postgres", false, 1024, |_, _, _| Ok(())).is_err());
        }
    }

    #[test]
    fn copy_terminator_can_be_the_last_line_without_newline() {
        let mut emitted = 0;
        statements(
            b"COPY t FROM STDIN;\n1\n\\.".as_slice(),
            "postgres",
            false,
            128,
            |_, _, skip| {
                assert!(skip);
                emitted += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(emitted, 1);
    }
}
