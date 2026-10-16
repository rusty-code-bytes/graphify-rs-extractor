use crate::{Stats, model::Node, store::Store};
use anyhow::{Context, Result, bail};
use sqlparser::{
    ast::{
        AlterTableOperation, ColumnOption, ConditionalStatements, Declare, DeclareAssignment,
        DeclareType, Expr, ObjectName, ObjectNamePart, Query, SetExpr, Statement, TableConstraint,
        TableFactor, Visit, Visitor,
    },
    dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    keywords::Keyword,
    parser::{IsOptional, Parser, ParserError},
    tokenizer::{Token, TokenWithSpan, Tokenizer},
};
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{BufReader, Bytes, Read},
    ops::ControlFlow,
    path::Path,
};

mod mysql_expressions;
mod mysql_modes;
mod mysql_programs;

// Preserve the MySQL dialect identity and overrides while filling specific
// grammar gaps. Every MySQL parse uses this wrapper so expression fixes and the
// current string-escape mode apply consistently, including nested queries.
#[derive(Debug)]
struct MySqlProgramDialect(MySqlDialect, bool);

macro_rules! delegate_mysql_flags {
    ($($method:ident),+ $(,)?) => {
        $(fn $method(&self) -> bool { self.0.$method() })+
    };
}

impl Dialect for MySqlProgramDialect {
    fn dialect(&self) -> std::any::TypeId {
        self.0.dialect()
    }
    fn is_identifier_start(&self, ch: char) -> bool {
        self.0.is_identifier_start(ch)
    }
    fn is_identifier_part(&self, ch: char) -> bool {
        self.0.is_identifier_part(ch)
    }
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        self.0.is_delimited_identifier_start(ch)
    }
    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        self.0.identifier_quote_style(identifier)
    }
    fn parse_infix(
        &self,
        parser: &mut Parser<'_>,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        mysql_expressions::parse_infix(parser, expr, precedence)
            .or_else(|| self.0.parse_infix(parser, expr, precedence))
    }
    fn parse_prefix(&self, parser: &mut Parser<'_>) -> Option<Result<Expr, ParserError>> {
        mysql_expressions::parse_prefix(parser).or_else(|| self.0.parse_prefix(parser))
    }
    fn get_next_precedence(&self, parser: &Parser<'_>) -> Option<Result<u8, ParserError>> {
        mysql_expressions::get_next_precedence(parser)
            .or_else(|| self.0.get_next_precedence(parser))
    }
    fn supports_string_literal_backslash_escape(&self) -> bool {
        !self.1
    }
    fn parse_column_option(
        &self,
        parser: &mut Parser<'_>,
    ) -> Result<Option<Result<Option<ColumnOption>, ParserError>>, ParserError> {
        if matches!(parser.peek_token().token, Token::Word(ref word)
            if word.quote_style.is_none() && (word.value.eq_ignore_ascii_case("INVISIBLE")
                || word.value.eq_ignore_ascii_case("VISIBLE")))
        {
            return Ok(Some(Ok(Some(ColumnOption::DialectSpecific(vec![
                parser.next_token().token,
            ])))));
        }
        self.0.parse_column_option(parser)
    }
    fn is_table_factor_alias(&self, explicit: bool, kw: &Keyword, parser: &mut Parser<'_>) -> bool {
        self.0.is_table_factor_alias(explicit, kw, parser)
    }
    delegate_mysql_flags!(
        supports_string_literal_concatenation,
        ignores_wildcard_escapes,
        supports_numeric_prefix,
        require_interval_qualifier,
        supports_limit_comma,
        supports_create_table_select,
        supports_insert_set,
        supports_user_host_grantee,
        supports_table_hints,
        requires_single_line_comment_whitespace,
        supports_match_against,
        supports_set_names,
        supports_comma_separated_set_assignments,
        supports_data_type_signed_suffix,
        supports_cross_join_constraint,
    );
    fn parse_statement(&self, parser: &mut Parser<'_>) -> Option<Result<Statement, ParserError>> {
        if parser.parse_keyword(Keyword::DECLARE) {
            Some(parse_mysql_declare(parser))
        } else {
            self.0.parse_statement(parser)
        }
    }
}

fn parse_mysql_declare(parser: &mut Parser<'_>) -> Result<Statement, ParserError> {
    let names = parser.parse_comma_separated(|parser| {
        if !matches!(&parser.peek_token().token, Token::Word(word)
            if !word.value.starts_with('@'))
        {
            return parser.expected("a MySQL local variable name", parser.peek_token());
        }
        parser.parse_identifier()
    })?;
    let mut declaration = Declare {
        names,
        data_type: None,
        assignment: None,
        declare_type: None,
        binary: None,
        sensitive: None,
        scroll: None,
        hold: None,
        for_query: None,
    };
    if declaration.names.len() == 1 && parser.parse_keyword(Keyword::CURSOR) {
        parser.expect_keyword_is(Keyword::FOR)?;
        declaration.declare_type = Some(DeclareType::Cursor);
        declaration.for_query = Some(parser.parse_query()?);
    } else {
        declaration.data_type = Some(parse_mysql_local_type(parser)?);
        if parser.parse_keyword(Keyword::DEFAULT) {
            declaration.assignment =
                Some(DeclareAssignment::Default(Box::new(parser.parse_expr()?)));
        }
    }
    Ok(Statement::Declare {
        stmts: vec![declaration],
    })
}

fn parse_mysql_local_type(
    parser: &mut Parser<'_>,
) -> Result<sqlparser::ast::DataType, ParserError> {
    // MySQL has no user-defined local variable types. Do not let the generic
    // parser treat missing types (e.g. DEFAULT) as arbitrary custom names.
    if !matches!(&parser.peek_token().token, Token::Word(word)
        if word.quote_style.is_none() && matches!(word.keyword,
            Keyword::BIT | Keyword::TINYINT | Keyword::SMALLINT | Keyword::MEDIUMINT
            | Keyword::INT | Keyword::INTEGER | Keyword::BIGINT | Keyword::REAL
            | Keyword::DOUBLE | Keyword::FLOAT | Keyword::DECIMAL | Keyword::DEC
            | Keyword::NUMERIC | Keyword::DATE | Keyword::TIME | Keyword::TIMESTAMP
            | Keyword::DATETIME | Keyword::YEAR | Keyword::CHAR | Keyword::CHARACTER
            | Keyword::VARCHAR | Keyword::BINARY | Keyword::VARBINARY | Keyword::TINYBLOB
            | Keyword::BLOB | Keyword::MEDIUMBLOB | Keyword::LONGBLOB | Keyword::TINYTEXT
            | Keyword::TEXT | Keyword::MEDIUMTEXT | Keyword::LONGTEXT | Keyword::ENUM
            | Keyword::SET | Keyword::JSON | Keyword::BOOL | Keyword::BOOLEAN))
    {
        return parser.expected("a supported MySQL local variable type", parser.peek_token());
    }
    parser.parse_data_type()
}

// The parser's DECLARE hook has no enclosing-block context. Validate the AST
// after parsing so the compatibility path cannot admit declarations at file
// scope, in an IF branch, or after executable statements. Nested declaration
// blocks need the full mysql_programs grammar: the native conditional parser
// does not enforce MySQL's required semicolon after the nested END.
fn validate_mysql_declarations(statements: &[Statement]) -> Result<(), ParserError> {
    fn block(body: &ConditionalStatements) -> Result<usize, ParserError> {
        if !matches!(body, ConditionalStatements::BeginEnd(_)) {
            return Ok(0);
        }
        let mut count = 0;
        let mut executable = false;
        let mut cursor = false;
        let mut names = HashSet::new();
        for statement in body.statements() {
            let Statement::Declare { stmts } = statement else {
                executable = true;
                continue;
            };
            count += 1;
            for declaration in stmts {
                for name in &declaration.names {
                    let message = if executable {
                        Some("MySQL DECLARE must precede executable statements")
                    } else if cursor && declaration.data_type.is_some() {
                        Some("MySQL local variables must precede cursor declarations")
                    } else if !names.insert((
                        declaration.declare_type.is_some(),
                        name.value.to_lowercase(),
                    )) {
                        Some("duplicate MySQL local declaration")
                    } else {
                        None
                    };
                    if let Some(message) = message {
                        return Err(ParserError::ParserError(format!(
                            "{message} at Line: {}, Column: {}",
                            name.span.start.line, name.span.start.column
                        )));
                    }
                }
                cursor |= declaration.declare_type == Some(DeclareType::Cursor);
            }
        }
        Ok(count)
    }
    #[derive(Default)]
    struct Declarations {
        scoped: usize,
        total: usize,
    }
    impl Visitor for Declarations {
        type Break = ParserError;
        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            let body = match statement {
                Statement::CreateTrigger(trigger) => trigger.statements.as_ref(),
                Statement::CreateProcedure { body, .. } => Some(body),
                Statement::Declare { .. } => {
                    self.total += 1;
                    None
                }
                _ => None,
            };
            if let Some(body) = body {
                match block(body) {
                    Ok(count) => self.scoped += count,
                    Err(error) => return ControlFlow::Break(error),
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut declarations = Declarations::default();
    for statement in statements {
        if let ControlFlow::Break(error) = statement.visit(&mut declarations) {
            return Err(error);
        }
    }
    if declarations.total != declarations.scoped {
        return Err(ParserError::ParserError(
            "MySQL DECLARE requires the start of the trigger/procedure BEGIN/END block; nested declaration blocks are unsupported".into(),
        ));
    }
    Ok(())
}

// MySQL permits INSERT ... VALUE as a synonym for VALUES, but sqlparser 0.59
// only accepts VALUES. Locate that token through the INSERT header grammar:
// identifiers, expressions, strings and quoted VALUE tokens must stay intact.
// Keep original spans so the compatibility retry cannot shift query provenance.
fn mysql_insert_value_tokens(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Option<Vec<TokenWithSpan>>, ParserError> {
    let tokens = Tokenizer::new(dialect, sql).tokenize_with_location()?;
    let token_count = tokens.len();
    let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens);
    let mut positions = Vec::new();
    while parser.peek_token().token != Token::EOF {
        if !parser.parse_keyword(Keyword::INSERT) {
            parser.next_token();
            continue;
        }
        if let Some(position) = parser.maybe_parse(|parser| {
            let _ = parser.parse_one_of_keywords(&[
                Keyword::LOW_PRIORITY,
                Keyword::DELAYED,
                Keyword::HIGH_PRIORITY,
            ]);
            let _ = parser.parse_keyword(Keyword::IGNORE);
            let _ = parser.parse_keyword(Keyword::INTO);
            parser.parse_object_name(false)?;
            parser.parse_parenthesized_column_list(IsOptional::Optional, true)?;
            parser.parse_insert_partition()?;
            parser.expect_keyword_is(Keyword::VALUE)?;
            let position = parser.get_current_index();
            parser.expect_token(&Token::LParen)?;
            Ok(position)
        })? {
            positions.push(position);
        }
    }
    if positions.is_empty() {
        return Ok(None);
    }
    let mut tokens: Vec<_> = (0..token_count)
        .map(|index| parser.token_at(index).clone())
        .collect();
    for position in positions {
        if let Token::Word(word) = &mut tokens[position].token {
            word.value = "VALUES".into();
            word.keyword = Keyword::VALUES;
        }
    }
    Ok(Some(tokens))
}

fn parse_schema_statement(
    dialect: &dyn Dialect,
    dialect_name: &str,
    sql: &str,
    no_backslash_escapes: bool,
) -> Result<Vec<Statement>, ParserError> {
    if dialect_name != "mysql" {
        return Parser::parse_sql(dialect, sql);
    }
    let mysql = MySqlProgramDialect(MySqlDialect {}, no_backslash_escapes);
    let statements = match mysql_insert_value_tokens(&mysql, sql)? {
        Some(tokens) => Parser::new(&mysql)
            .with_tokens_with_locations(tokens)
            .parse_statements(),
        None => Parser::parse_sql(&mysql, sql),
    }?;
    validate_mysql_declarations(&statements)?;
    mysql_expressions::validate(&statements)?;
    validate_mysql_visibility(&statements)?;
    Ok(statements)
}

fn validate_mysql_visibility(statements: &[Statement]) -> Result<(), ParserError> {
    for statement in statements {
        let Statement::CreateTable(table) = statement else {
            continue;
        };
        let mut visible = false;
        for column in &table.columns {
            let mut visibility = None;
            for option in &column.options {
                if let ColumnOption::DialectSpecific(tokens) = &option.option {
                    for token in tokens {
                        if let Token::Word(word) = token
                            && (word.value.eq_ignore_ascii_case("INVISIBLE")
                                || word.value.eq_ignore_ascii_case("VISIBLE"))
                        {
                            if visibility.is_some() {
                                return Err(ParserError::ParserError(format!(
                                    "duplicate column visibility at Line: {}, Column: {}",
                                    column.name.span.start.line, column.name.span.start.column
                                )));
                            }
                            visibility = Some(word.value.eq_ignore_ascii_case("VISIBLE"));
                        }
                    }
                }
            }
            visible |= visibility != Some(false);
        }
        if !table.columns.is_empty() && !visible {
            return Err(ParserError::ParserError(
                "MySQL tables require a visible column".into(),
            ));
        }
    }
    Ok(())
}

// Account-management statements have no table/schema projection. sqlparser's
// generic CREATE USER grammar does not accept MySQL's user@host form. Validate
// the supported complete syntax before counting it as intentionally ignored.
fn mysql_account_control(dialect: &dyn Dialect, sql: &str) -> Result<bool> {
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
    let mut parser = Parser::new(dialect).try_with_sql(sql)?;
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
    mut emit: impl FnMut(&[u8], usize, bool, bool) -> Result<()>,
) -> Result<()> {
    let mut input = Input::new(reader);
    let mut buffer = Vec::new();
    let mut skip = false;
    let mut first_word_done = false;
    let mut line = 1;
    let mut delimiter = b";".to_vec();
    let mut executable_comment = false;
    let mut modes = mysql_modes::MysqlModes::default();
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
                emit(&[], line, true, modes.no_backslash_escapes())?;
            } else if !buffer.iter().all(u8::is_ascii_whitespace) {
                let no_backslash = modes.no_backslash_escapes();
                if dialect == "mysql" {
                    let sql = std::str::from_utf8(&buffer)
                        .context("SQL schema statements must be UTF-8")?;
                    modes
                        .apply(sql, &MySqlProgramDialect(MySqlDialect {}, no_backslash))
                        .with_context(|| format!("MySQL session setting near line {line}"))?;
                }
                emit(&buffer, line, false, no_backslash)?;
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
            let mut ended_line = false;
            while let Some(c) = input.next()? {
                if c == b'\n' {
                    ended_line = true;
                    break;
                }
            }
            if !buffer.is_empty() {
                append(
                    &mut buffer,
                    if ended_line { b'\n' } else { b' ' },
                    skip,
                    limit,
                )?;
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
            if !buffer.is_empty() {
                append(&mut buffer, b' ', skip, limit)?;
            }
            let mut nesting = 1usize;
            while nesting > 0 {
                if input.starts(b"*/")? {
                    input.eat(2)?;
                    nesting -= 1;
                } else if dialect == "postgres" && input.starts(b"/*")? {
                    input.eat(2)?;
                    nesting += 1;
                } else {
                    match input.next()? {
                        None => bail!("unterminated SQL comment near line {line}"),
                        Some(b'\n') if !buffer.is_empty() => {
                            append(&mut buffer, b'\n', skip, limit)?;
                        }
                        _ => {}
                    }
                }
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
                    // mysql's com_delimiter copies the full command into a
                    // 255-byte buffer before parsing its first argument.
                    if value.len() < 255 - "DELIMITER".len() {
                        value.push(c);
                    }
                }
                delimiter = mysql_modes::delimiter(&value)?;
                buffer.clear();
                continue;
            }
            if matches!(keyword.as_str(), "INSERT" | "REPLACE") {
                skip = true;
                buffer.clear();
            }
            first_word_done = true;
        }
        // Data payloads are streamed without building an AST. An assignment
        // outside a quoted literal makes saved session values unknowable.
        if dialect == "mysql" && skip && input.starts(b":=")? {
            modes.invalidate_saved();
        }
        if matches!(b, b'\'' | b'"' | b'`') || (b == b'[' && dialect == "sqlite") {
            let quote = if b == b'[' { b']' } else { b };
            let backslash = (dialect == "mysql" && b != b'`' && !modes.no_backslash_escapes())
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
        emit(&[], line, true, modes.no_backslash_escapes())?;
    } else if !buffer.iter().all(u8::is_ascii_whitespace) {
        let no_backslash = modes.no_backslash_escapes();
        if dialect == "mysql" {
            let sql =
                std::str::from_utf8(&buffer).context("SQL schema statements must be UTF-8")?;
            modes
                .apply(sql, &MySqlProgramDialect(MySqlDialect {}, no_backslash))
                .with_context(|| format!("MySQL session setting near line {line}"))?;
        }
        emit(&buffer, line, false, no_backslash)?;
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

// Query callbacks keep CTE visibility lexical. In a nonrecursive WITH, a CTE's
// definition sees earlier siblings, while the main query sees all siblings.
// A nested WITH must not leak its names into a later sibling/subquery.
struct QueryScope {
    ctes: Vec<(usize, String)>,
    visible: usize,
    recursive: bool,
}

struct ViewReads<'a, F> {
    dialect: &'a str,
    scopes: Vec<QueryScope>,
    emit: F,
}

impl<F: FnMut(&ObjectName) -> Result<()>> Visitor for ViewReads<'_, F> {
    type Break = anyhow::Error;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        fn has_table_query(body: &SetExpr) -> bool {
            match body {
                SetExpr::Table(_) => true,
                SetExpr::SetOperation { left, right, .. } => {
                    has_table_query(left) || has_table_query(right)
                }
                _ => false,
            }
        }
        // sqlparser's TABLE query stores names as strings, losing quoting and
        // token provenance. Refuse that form rather than inventing a binding.
        if has_table_query(&query.body) {
            return ControlFlow::Break(anyhow::anyhow!(
                "SQL view TABLE query lacks identifier provenance; use SELECT FROM"
            ));
        }
        if let Some(parent) = self.scopes.last_mut()
            && let Some(i) = parent
                .ctes
                .iter()
                .position(|(ptr, _)| *ptr == query as *const Query as usize)
        {
            parent.visible =
                if self.dialect == "sqlite" || (parent.recursive && self.dialect != "mysql") {
                    parent.ctes.len()
                } else {
                    i + usize::from(parent.recursive)
                };
        }
        let mut ctes = Vec::new();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let name = ObjectName(vec![ObjectNamePart::Identifier(cte.alias.name.clone())]);
                let key = match object_key(&name, self.dialect) {
                    Ok(key) => key,
                    Err(error) => return ControlFlow::Break(error),
                };
                if ctes.iter().any(|(_, prior)| prior == &key) {
                    return ControlFlow::Break(anyhow::anyhow!("duplicate SQL CTE name"));
                }
                ctes.push((cte.query.as_ref() as *const Query as usize, key));
            }
        }
        self.scopes.push(QueryScope {
            visible: ctes.len(),
            ctes,
            recursive: query.with.as_ref().is_some_and(|with| with.recursive),
        });
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.scopes.pop();
        if let Some(parent) = self.scopes.last_mut()
            && parent
                .ctes
                .iter()
                .any(|(ptr, _)| *ptr == query as *const Query as usize)
        {
            parent.visible = parent.ctes.len();
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        // Aliases and function names are not physical relations. Walking only
        // table factors also avoids mistaking column qualifiers for FROM names.
        if let TableFactor::Table {
            name, args: None, ..
        } = factor
        {
            let key = match object_key(name, self.dialect) {
                Ok(key) => key,
                Err(error) => return ControlFlow::Break(error),
            };
            if name.0.len() == 1
                && self.scopes.iter().rev().any(|scope| {
                    scope.ctes[..scope.visible]
                        .iter()
                        .any(|(_, cte)| cte == &key)
                })
            {
                return ControlFlow::Continue(());
            }
            if let Err(error) = (self.emit)(name) {
                return ControlFlow::Break(error);
            }
        }
        ControlFlow::Continue(())
    }
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
        |bytes, line, skipped, no_backslash_escapes| {
            if skipped {
                stats.skipped_sql_data_statements += 1;
                return Ok(());
            }
            let sql = std::str::from_utf8(bytes).context("SQL schema statements must be UTF-8")?;
            let mysql_dialect = MySqlProgramDialect(MySqlDialect {}, no_backslash_escapes);
            let current_dialect: &dyn Dialect = if dialect_name == "mysql" {
                &mysql_dialect
            } else {
                dialect.as_ref()
            };
            let keyword = sql
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if dialect_name == "mysql" && mysql_account_control(&mysql_dialect, sql)? {
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
                if sql.to_ascii_lowercase().contains("search_path") {
                    bail!(
                        "session setting changes SQL resolution/lexing near line {line}; normalize the dump first"
                    );
                }
                stats.ignored_sql_statements += 1;
                return Ok(());
            }
            if keyword == "USE" {
                let parsed = Parser::parse_sql(current_dialect, sql)
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
            let mut extra = Parser::new(current_dialect).try_with_sql(sql)?;
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
            let mut extra = Parser::new(current_dialect).try_with_sql(sql)?;
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
            let program = dialect_name == "mysql"
                && mysql_programs::validate(&mysql_dialect, sql)
                    .with_context(|| format!("MySQL stored-program parse near line {line}"))?;
            let parsed =
                parse_schema_statement(dialect.as_ref(), dialect_name, sql, no_backslash_escapes);
            let statements = match parsed {
                Ok(statements) => statements,
                // The fallback is available only after the entire program has
                // passed the explicit grammar, DML and expression checks above.
                Err(_) if program => {
                    let mut node = Node::new(
                        file,
                        "sql_statement",
                        &format!(
                            "{}@{line}",
                            sql.split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(160)
                                .collect::<String>()
                        ),
                        "sql",
                        line,
                    );
                    node.extraction_coverage = Some("statement_only".into());
                    stats.sql_statement_only += 1;
                    store.node(&node)?;
                    store.edge(file_id, &node.id, "contains", file, line)?;
                    return Ok(());
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("SQL parse near line {line}"));
                }
            };
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
                    Statement::CreateView { name, query, .. } => {
                        let mut node = Node::new(file, "view", &key(&name)?, "sql", line);
                        node.label = name.to_string();
                        store.node(&node)?;
                        store.sql_definition(file, &key(&name)?, &node.id)?;
                        store.edge(file_id, &node.id, "defines", file, line)?;
                        let mut visitor = ViewReads {
                            dialect: dialect_name,
                            scopes: Vec::new(),
                            emit: |target: &ObjectName| {
                                let target_line = target.0.first().and_then(|part| match part {
                                    ObjectNamePart::Identifier(ident) => {
                                        usize::try_from(ident.span.start.line).ok()
                                    }
                                    _ => None,
                                });
                                store.reference(
                                    &node.id,
                                    "sql-relation",
                                    &key(target)?,
                                    None,
                                    "reads_from",
                                    file,
                                    line + target_line.unwrap_or(1).saturating_sub(1),
                                )
                            },
                        };
                        if let ControlFlow::Break(error) = query.visit(&mut visitor) {
                            return Err(error)
                                .with_context(|| format!("SQL view query near line {line}"));
                        }
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
                            match operation {
                                AlterTableOperation::AddConstraint {
                                    constraint: TableConstraint::ForeignKey { foreign_table, .. },
                                    ..
                                } => foreign_key(&node.id, &name, &foreign_table)?,
                                AlterTableOperation::AddColumn { column_def, .. } => {
                                    for option in column_def.options {
                                        if let ColumnOption::ForeignKey { foreign_table, .. } =
                                            option.option
                                        {
                                            foreign_key(&node.id, &name, &foreign_table)?;
                                        }
                                    }
                                }
                                _ => {}
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
                        let mut node = Node::new(
                            file,
                            "sql_statement",
                            &format!(
                                "{}@{line}",
                                other.to_string().chars().take(160).collect::<String>()
                            ),
                            "sql",
                            line,
                        );
                        node.extraction_coverage = Some("statement_only".into());
                        stats.sql_statement_only += 1;
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
    fn mysql_local_declarations_retain_ast_types_defaults_and_spans() {
        let sql = "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN\n\
  DECLARE vBalance DECIMAL(18,2);\n\
  DECLARE vDate DATE DEFAULT current_date();\n\
  DECLARE vOldCase, vNewCase INT SIGNED DEFAULT (9 DIV 2);\n\
  DECLARE `value` VARCHAR(32) DEFAULT 'it\\'s' ' concatenated';\n\
  DECLARE rows_cursor CURSOR FOR SELECT id FROM source LIMIT 1, 2;\n\
  SET vOldCase = 0, vNewCase = 1;\n\
  INSERT INTO `value` SET `value` = 0x12, note = 'quoted\\'text';\n\
  SELECT `value` FROM `value` USE INDEX (idx) WHERE `value` = 1 DIV 2 LIMIT 1, 2;\n\
END";
        assert!(Parser::parse_sql(&MySqlDialect {}, sql).is_err());
        let statements = parse_schema_statement(&MySqlDialect {}, "mysql", sql, false).unwrap();
        let Statement::CreateTrigger(trigger) = &statements[0] else {
            panic!("expected trigger");
        };
        let body = trigger.statements.as_ref().unwrap().statements();
        for (index, names, data_type, assignment) in [
            (0, vec!["vBalance"], "DECIMAL(18,2)", None),
            (1, vec!["vDate"], "DATE", Some("DEFAULT current_date()")),
            (
                2,
                vec!["vOldCase", "vNewCase"],
                "INT",
                Some("DEFAULT (9 DIV 2)"),
            ),
            (
                3,
                vec!["value"],
                "VARCHAR(32)",
                Some("DEFAULT 'it''s concatenated'"),
            ),
        ] {
            let Statement::Declare { stmts } = &body[index] else {
                panic!("expected local declaration");
            };
            assert_eq!(stmts.len(), 1);
            assert_eq!(
                stmts[0]
                    .names
                    .iter()
                    .map(|name| name.value.as_str())
                    .collect::<Vec<_>>(),
                names
            );
            assert_eq!(stmts[0].data_type.as_ref().unwrap().to_string(), data_type);
            assert_eq!(
                stmts[0]
                    .assignment
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref(),
                assignment
            );
            assert_eq!(stmts[0].names[0].span.start.line, index as u64 + 2);
            assert_eq!(stmts[0].names[0].span.start.column, 9);
        }
        let Statement::Declare { stmts } = &body[4] else {
            panic!("expected cursor declaration after variables");
        };
        assert_eq!(stmts[0].declare_type, Some(DeclareType::Cursor));
        assert_eq!(
            stmts[0].for_query.as_ref().unwrap().to_string(),
            "SELECT id FROM source LIMIT 1, 2"
        );
        assert_eq!(body.len(), 8);
    }

    #[test]
    fn mysql_program_dialect_preserves_existing_mysql_parsing() {
        let wrapped = MySqlProgramDialect(MySqlDialect {}, false);
        assert!((&wrapped as &dyn Dialect).is::<MySqlDialect>());
        for sql in [
            "SELECT 7 DIV 2, 0x12, 'escaped\\'quote' ' concatenated', `value` FROM `value` USE INDEX (idx) LIMIT 1, 2",
            "SET x = 1, y = 2",
            "SET NAMES utf8mb4",
            "CREATE TABLE `value`(id INT SIGNED, note VARCHAR(8)) AS SELECT 1, 'note'",
            "INSERT INTO `value` SET id = 1, note = 'note'",
            "LOCK TABLES source READ",
            "UNLOCK TABLES",
            "SELECT MATCH(note) AGAINST('needle') FROM source",
            "SELECT * FROM source CROSS JOIN other ON source.id = other.id",
        ] {
            assert_eq!(
                Parser::parse_sql(&wrapped, sql).unwrap(),
                Parser::parse_sql(&MySqlDialect {}, sql).unwrap(),
                "{sql}"
            );
        }
        let sql = "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN DECLARE vBalance DECIMAL(18,2); END";
        for (name, dialect) in [
            ("generic", Box::new(GenericDialect {}) as Box<dyn Dialect>),
            ("postgres", Box::new(PostgreSqlDialect {})),
            ("sqlite", Box::new(SQLiteDialect {})),
        ] {
            assert!(
                parse_schema_statement(dialect.as_ref(), name, sql, false).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn mysql_declaration_fallback_preserves_error_line_and_column() {
        for (sql, found) in [
            (
                "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN\n  DECLARE vBalance DECIMAL(18,);\nEND",
                ")",
            ),
            (
                "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN\n  DECLARE vBalance DECIMAL(18,2);\n  INSERT INTO audit_log VALUE(NEW.id) nonsense;\nEND",
                "nonsense",
            ),
        ] {
            let error = parse_schema_statement(&MySqlDialect {}, "mysql", sql, false).unwrap_err();
            let (line_index, line) = sql
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(found))
                .unwrap();
            let column = line.find(found).unwrap() + 1;
            assert!(
                error
                    .to_string()
                    .contains(&format!("at Line: {}, Column: {column}", line_index + 1)),
                "{error}"
            );
        }
    }

    #[test]
    fn mysql_insert_value_retry_changes_only_row_source_tokens() {
        let sql = "CREATE TRIGGER t AFTER UPDATE ON value FOR EACH ROW BEGIN\n\
INSERT LOW_PRIORITY IGNORE INTO value(value, `VALUE`, note) vAlUe /* VALUE */\n\
(NEW.value, NEW.`VALUE`, 'VALUE');\n\
INSERT INTO `VALUE`(value) VALUES ('value');\n\
INSERT audit_log(note) VALUE (concat('VALUE', NEW.value));\n\
END";
        let expected = "CREATE TRIGGER t AFTER UPDATE ON value FOR EACH ROW BEGIN\n\
INSERT LOW_PRIORITY IGNORE INTO value(value, `VALUE`, note) VALUES /* VALUE */\n\
(NEW.value, NEW.`VALUE`, 'VALUE');\n\
INSERT INTO `VALUE`(value) VALUES ('value');\n\
INSERT audit_log(note) VALUES (concat('VALUE', NEW.value));\n\
END";
        let dialect = MySqlDialect {};
        assert!(Parser::parse_sql(&dialect, sql).is_err());
        let parsed = parse_schema_statement(&dialect, "mysql", sql, false).unwrap();
        let control = Parser::parse_sql(&dialect, expected).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].to_string(), control[0].to_string());

        let original = Tokenizer::new(&dialect, sql)
            .tokenize_with_location()
            .unwrap();
        let normalized = mysql_insert_value_tokens(&MySqlDialect {}, sql)
            .unwrap()
            .unwrap();
        assert_eq!(original.len(), normalized.len());
        let mut changes = 0;
        for (before, after) in original.iter().zip(normalized.iter()) {
            assert_eq!(before.span, after.span);
            if before.token != after.token {
                changes += 1;
                assert!(matches!(&before.token, Token::Word(word)
                    if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("VALUE")));
                assert!(matches!(&after.token, Token::Word(word)
                    if word.keyword == Keyword::VALUES));
            }
        }
        assert_eq!(changes, 2);
    }

    #[test]
    fn mysql_insert_value_retry_does_not_extend_other_query_syntax_or_dialects() {
        for sql in [
            "SELECT value(1)",
            "INSERT INTO audit_log `VALUE` (1)",
            "INSERT INTO audit_log SELECT VALUE (1)",
            "INSERT INTO audit_log VALUES (value(1))",
            "CREATE TABLE value(value INT DEFAULT (value(1)))",
        ] {
            assert!(
                mysql_insert_value_tokens(&MySqlDialect {}, sql)
                    .unwrap()
                    .is_none(),
                "{sql}"
            );
        }
        let sql = "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO audit_log VALUE (NEW.id); END";
        assert!(parse_schema_statement(&MySqlDialect {}, "mysql", sql, false).is_ok());
        for (name, dialect) in [
            ("generic", Box::new(GenericDialect {}) as Box<dyn Dialect>),
            ("postgres", Box::new(PostgreSqlDialect {})),
            ("sqlite", Box::new(SQLiteDialect {})),
        ] {
            assert!(parse_schema_statement(dialect.as_ref(), name, sql, false).is_err());
        }
    }

    #[test]
    fn mysql_insert_value_retry_keeps_error_columns_after_the_rewritten_token() {
        let sql = "CREATE TRIGGER t AFTER UPDATE ON source FOR EACH ROW BEGIN INSERT INTO audit_log VALUE(NEW.id) nonsense; END";
        let error = parse_schema_statement(&MySqlDialect {}, "mysql", sql, false).unwrap_err();
        let column = sql.find("nonsense").unwrap() + 1;
        assert!(
            error
                .to_string()
                .contains(&format!("nonsense at Line: 1, Column: {column}")),
            "{error}"
        );
    }

    #[test]
    fn auto_comments_and_strict_mysql_arithmetic_are_distinct() {
        let sql = b"SELECT 4--1;";
        let mut strict = Vec::new();
        statements(sql.as_slice(), "mysql", false, 128, |s, _, _, _| {
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
            |s, _, _, _| {
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
        statements(sql.as_bytes(), "mysql", false, 128, |s, _, skip, _| {
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
            |_, _, _, _| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 2);
        let mut count = 0;
        statements(b"DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT ';'; END$$\nDELIMITER ;\nCREATE TABLE t(x int);".as_slice(),"mysql",false,1024,|_,_,_,_|{ count+=1; Ok(()) }).unwrap();
        assert_eq!(count, 2);
        let mut count = 0;
        statements(
            b"SELECT $tag$a;b$tag$; SELECT 2;".as_slice(),
            "postgres",
            false,
            1024,
            |_, _, _, _| {
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
            assert!(
                statements(sql.as_bytes(), "postgres", false, 1024, |_, _, _, _| Ok(())).is_err()
            );
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
            |_, _, skip, _| {
                assert!(skip);
                emitted += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(emitted, 1);
    }
}
