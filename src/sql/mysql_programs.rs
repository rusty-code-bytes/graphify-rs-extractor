//! A bounded MySQL stored-program grammar for constructs absent from sqlparser.
//!
//! This validates a complete program, including its DML and expressions, but
//! does not project body dependencies. The caller records statement-only
//! coverage when a complete native AST is unavailable.
use super::{mysql_expressions, mysql_insert_value_tokens, parse_mysql_declare};
use sqlparser::{
    ast::{DeclareType, Expr, Ident, Statement, UnaryOperator, Value},
    dialect::Dialect,
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::Token,
};
use std::collections::HashSet;

#[derive(Default)]
struct Scope {
    variables: HashSet<String>,
    conditions: HashSet<String>,
    signal_conditions: HashSet<String>,
    cursors: HashSet<String>,
    declaration_rank: u8,
    executable: bool,
}

struct Program<'a, 'd> {
    parser: &'a mut Parser<'d>,
    scopes: Vec<Scope>,
    labels: Vec<(String, bool)>,
    depth: usize,
    function: bool,
    returns: usize,
}

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

fn is_word(parser: &Parser<'_>, values: &[&str]) -> bool {
    matches!(parser.peek_token().token, Token::Word(ref w)
        if w.quote_style.is_none() && values.iter().any(|s| w.value.eq_ignore_ascii_case(s)))
}

impl Program<'_, '_> {
    fn expected<T>(&self, expected: &str) -> Result<T, ParserError> {
        self.parser.expected(expected, self.parser.peek_token())
    }

    fn expect_word(&mut self, value: &str) -> Result<(), ParserError> {
        if word(self.parser, value) {
            Ok(())
        } else {
            self.expected(value)
        }
    }

    fn identifier(&mut self) -> Result<Ident, ParserError> {
        if !matches!(self.parser.peek_token().token, Token::Word(ref w) if !w.value.starts_with('@'))
        {
            return self.expected("a MySQL program identifier");
        }
        self.parser.parse_identifier()
    }

    fn exists(&self, name: &str, kind: u8) -> bool {
        self.scopes.iter().rev().any(|scope| match kind {
            0 => scope.variables.contains(name),
            1 => scope.conditions.contains(name),
            _ => scope.cursors.contains(name),
        })
    }

    fn declare_name(&mut self, name: &Ident, kind: u8) -> Result<(), ParserError> {
        let scope = self.scopes.last_mut().expect("program scope");
        let names = match kind {
            0 => &mut scope.variables,
            1 => &mut scope.conditions,
            _ => &mut scope.cursors,
        };
        if !names.insert(name.value.to_lowercase()) {
            return self.expected("a unique declaration name in this block");
        }
        Ok(())
    }

    fn expression(&mut self) -> Result<(), ParserError> {
        let expr = self.parser.parse_expr()?;
        mysql_expressions::validate_expression(&expr)
    }

    fn signal_value(&mut self) -> Result<(), ParserError> {
        let expr = self.parser.parse_expr()?;
        mysql_expressions::validate_expression(&expr)?;
        let simple = match &expr {
            Expr::Value(_) => true,
            Expr::Prefixed { prefix, value } => {
                prefix.quote_style.is_none()
                    && prefix.value.starts_with('_')
                    && matches!(value.as_ref(), Expr::Value(value) if matches!(value.value, Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)))
            }
            Expr::Identifier(name) => {
                name.value.starts_with('@') || self.exists(&name.value.to_lowercase(), 0)
            }
            Expr::CompoundIdentifier(names) => names
                .first()
                .is_some_and(|name| name.value.starts_with("@@")),
            Expr::UnaryOp {
                op: UnaryOperator::Plus | UnaryOperator::Minus,
                expr,
            } => {
                matches!(expr.as_ref(), Expr::Value(value) if matches!(value.value, Value::Number(_, _)))
            }
            _ => false,
        };
        if simple {
            Ok(())
        } else {
            self.expected("a literal, parameter, local or session variable for SIGNAL")
        }
    }

    fn condition(&mut self, named: bool) -> Result<(), ParserError> {
        if word(self.parser, "SQLSTATE") {
            word(self.parser, "VALUE");
            match self.parser.next_token().token {
                Token::SingleQuotedString(value)
                    if value.len() == 5
                        && value.bytes().all(|b| b.is_ascii_alphanumeric())
                        && !value.starts_with("00") =>
                {
                    Ok(())
                }
                _ => self.expected("a five-character non-success SQLSTATE"),
            }
        } else if matches!(self.parser.peek_token().token, Token::Number(_, _)) {
            let token = self.parser.next_token();
            if matches!(token.token, Token::Number(ref n, false) if n.parse::<u16>().is_ok_and(|n| n > 0))
            {
                Ok(())
            } else {
                self.expected("a nonzero MySQL error number")
            }
        } else if named && (word(self.parser, "SQLWARNING") || word(self.parser, "SQLEXCEPTION")) {
            Ok(())
        } else if named && word(self.parser, "NOT") {
            self.expect_word("FOUND")
        } else if named {
            let name = self.identifier()?;
            if self.exists(&name.value.to_lowercase(), 1) {
                Ok(())
            } else {
                self.expected("a declared condition")
            }
        } else {
            self.expected("SQLSTATE or a MySQL error number")
        }
    }

    fn declaration(&mut self) -> Result<(), ParserError> {
        let scope = self.scopes.last().expect("program scope");
        if scope.executable {
            return self.expected("DECLARE before executable statements");
        }
        self.expect_word("DECLARE")?;
        if word(self.parser, "CONTINUE") || word(self.parser, "EXIT") {
            self.expect_word("HANDLER")?;
            self.expect_word("FOR")?;
            self.scopes.last_mut().unwrap().declaration_rank = 2;
            loop {
                self.condition(true)?;
                if !self.parser.consume_token(&Token::Comma) {
                    break;
                }
            }
            // A handler owns one statement, which may itself be a block. A
            // DECLARE needs an explicit nested BEGIN/END block here as usual.
            // Enclosing labels are outside the handler's scope; labels
            // declared inside the handler remain available to its statements.
            let enclosing_labels = std::mem::take(&mut self.labels);
            let result = self.statement();
            self.labels = enclosing_labels;
            return result;
        }
        if matches!(self.parser.peek_nth_token(1).token, Token::Word(ref w)
            if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("CONDITION"))
        {
            if self.scopes.last().unwrap().declaration_rank > 0 {
                return self.expected("condition declarations before cursors and handlers");
            }
            let name = self.identifier()?;
            self.expect_word("CONDITION")?;
            self.expect_word("FOR")?;
            let sqlstate = is_word(self.parser, &["SQLSTATE"]);
            self.condition(false)?;
            self.declare_name(&name, 1)?;
            if sqlstate {
                self.scopes
                    .last_mut()
                    .unwrap()
                    .signal_conditions
                    .insert(name.value.to_lowercase());
            }
            return Ok(());
        }
        let statement = parse_mysql_declare(self.parser)?;
        mysql_expressions::validate(std::slice::from_ref(&statement))?;
        let Statement::Declare { stmts } = statement else {
            unreachable!()
        };
        for declaration in stmts {
            let cursor = declaration.declare_type == Some(DeclareType::Cursor);
            let rank = u8::from(cursor);
            if rank < self.scopes.last().unwrap().declaration_rank {
                return self.expected("variables/conditions before cursors, then handlers");
            }
            self.scopes.last_mut().unwrap().declaration_rank = rank;
            for name in declaration.names {
                self.declare_name(&name, if cursor { 2 } else { 0 })?;
            }
        }
        Ok(())
    }

    fn sequence(&mut self, until: &[&str], declarations: bool) -> Result<usize, ParserError> {
        let mut count = 0;
        while !is_word(self.parser, until) {
            if is_word(self.parser, &["DECLARE"]) && declarations {
                self.declaration()?;
            } else {
                if declarations {
                    self.scopes.last_mut().unwrap().executable = true;
                }
                self.statement()?;
            }
            self.parser.expect_token(&Token::SemiColon)?;
            count += 1;
        }
        Ok(count)
    }

    fn nonempty_sequence(&mut self, until: &[&str]) -> Result<(), ParserError> {
        if self.sequence(until, false)? == 0 {
            return self.expected("a stored-program statement");
        }
        Ok(())
    }

    fn end_label(&mut self, label: Option<&str>) -> Result<(), ParserError> {
        if let Some(label) = label
            && matches!(self.parser.peek_token().token, Token::Word(_))
        {
            let end = self.identifier()?;
            if !end.value.eq_ignore_ascii_case(label) {
                return self.expected("the opening block/loop label");
            }
        }
        Ok(())
    }

    fn statement(&mut self) -> Result<(), ParserError> {
        self.depth += 1;
        if self.depth > 32 {
            return self.expected("stored-program nesting at most 32 levels");
        }
        let result = self.statement_inner();
        self.depth -= 1;
        result
    }

    fn statement_inner(&mut self) -> Result<(), ParserError> {
        let label = if matches!(self.parser.peek_token().token, Token::Word(_))
            && self.parser.peek_nth_token(1).token == Token::Colon
        {
            let label = self.identifier()?.value.to_lowercase();
            self.parser.expect_token(&Token::Colon)?;
            if self.labels.iter().any(|(name, _)| name == &label) {
                return self.expected("a unique enclosing label");
            }
            Some(label)
        } else {
            None
        };
        let loop_statement = is_word(self.parser, &["LOOP", "WHILE", "REPEAT"]);
        if label.is_some() && !loop_statement && !is_word(self.parser, &["BEGIN"]) {
            return self.expected("BEGIN, LOOP, WHILE, or REPEAT after a label");
        }
        if let Some(label) = &label {
            self.labels.push((label.clone(), loop_statement));
        }
        let result = self.statement_body(label.as_deref());
        if label.is_some() {
            self.labels.pop();
        }
        result
    }

    fn statement_body(&mut self, label: Option<&str>) -> Result<(), ParserError> {
        if word(self.parser, "BEGIN") {
            self.scopes.push(Scope::default());
            self.sequence(&["END"], true)?;
            self.expect_word("END")?;
            self.scopes.pop();
            self.end_label(label)
        } else if word(self.parser, "IF") {
            self.expression()?;
            self.expect_word("THEN")?;
            self.nonempty_sequence(&["ELSEIF", "ELSE", "END"])?;
            while word(self.parser, "ELSEIF") {
                self.expression()?;
                self.expect_word("THEN")?;
                self.nonempty_sequence(&["ELSEIF", "ELSE", "END"])?;
            }
            if word(self.parser, "ELSE") {
                self.nonempty_sequence(&["END"])?;
            }
            self.expect_word("END")?;
            self.expect_word("IF")
        } else if word(self.parser, "CASE") {
            if !is_word(self.parser, &["WHEN"]) {
                self.expression()?;
            }
            self.expect_word("WHEN")?;
            loop {
                self.expression()?;
                self.expect_word("THEN")?;
                self.nonempty_sequence(&["WHEN", "ELSE", "END"])?;
                if !word(self.parser, "WHEN") {
                    break;
                }
            }
            if word(self.parser, "ELSE") {
                self.nonempty_sequence(&["END"])?;
            }
            self.expect_word("END")?;
            self.expect_word("CASE")
        } else if word(self.parser, "WHILE") {
            self.expression()?;
            self.expect_word("DO")?;
            self.sequence(&["END"], false)?;
            self.expect_word("END")?;
            self.expect_word("WHILE")?;
            self.end_label(label)
        } else if word(self.parser, "REPEAT") {
            self.sequence(&["UNTIL"], false)?;
            self.expect_word("UNTIL")?;
            self.expression()?;
            self.expect_word("END")?;
            self.expect_word("REPEAT")?;
            self.end_label(label)
        } else if word(self.parser, "LOOP") {
            self.sequence(&["END"], false)?;
            self.expect_word("END")?;
            self.expect_word("LOOP")?;
            self.end_label(label)
        } else if is_word(self.parser, &["LEAVE", "ITERATE"]) {
            let iterate = word(self.parser, "ITERATE");
            if !iterate {
                self.expect_word("LEAVE")?;
            }
            let name = self.identifier()?.value.to_lowercase();
            if self
                .labels
                .iter()
                .rev()
                .any(|(label, looping)| label == &name && (!iterate || *looping))
            {
                Ok(())
            } else {
                self.expected("an enclosing label of the appropriate kind")
            }
        } else if word(self.parser, "OPEN") || word(self.parser, "CLOSE") {
            let name = self.identifier()?.value.to_lowercase();
            if self.exists(&name, 2) {
                Ok(())
            } else {
                self.expected("a declared cursor")
            }
        } else if word(self.parser, "FETCH") {
            if word(self.parser, "NEXT") {
                self.expect_word("FROM")?;
            } else {
                word(self.parser, "FROM");
            }
            let name = self.identifier()?.value.to_lowercase();
            if !self.exists(&name, 2) {
                return self.expected("a declared cursor");
            }
            self.expect_word("INTO")?;
            loop {
                let name = self.identifier()?.value.to_lowercase();
                if !self.exists(&name, 0) {
                    return self.expected("a declared FETCH target variable");
                }
                if !self.parser.consume_token(&Token::Comma) {
                    break;
                }
            }
            Ok(())
        } else if word(self.parser, "RETURN") {
            if !self.function {
                return self.expected("RETURN only in a stored function");
            }
            self.expression()?;
            self.returns += 1;
            Ok(())
        } else if word(self.parser, "SIGNAL") {
            if is_word(self.parser, &["SQLSTATE"]) {
                self.condition(false)?;
            } else {
                let name = self.identifier()?.value.to_lowercase();
                if !self
                    .scopes
                    .iter()
                    .rev()
                    .find(|scope| scope.conditions.contains(&name))
                    .is_some_and(|scope| scope.signal_conditions.contains(&name))
                {
                    return self.expected("a declared SQLSTATE condition for SIGNAL");
                }
            }
            if word(self.parser, "SET") {
                let mut items = HashSet::new();
                loop {
                    let item = self.identifier()?.value.to_uppercase();
                    if !matches!(
                        item.as_str(),
                        "CLASS_ORIGIN"
                            | "SUBCLASS_ORIGIN"
                            | "MESSAGE_TEXT"
                            | "MYSQL_ERRNO"
                            | "CONSTRAINT_CATALOG"
                            | "CONSTRAINT_SCHEMA"
                            | "CONSTRAINT_NAME"
                            | "CATALOG_NAME"
                            | "SCHEMA_NAME"
                            | "TABLE_NAME"
                            | "COLUMN_NAME"
                            | "CURSOR_NAME"
                    ) {
                        return self.expected("a MySQL condition information item");
                    }
                    if !items.insert(item) {
                        return self.expected("a nonduplicate SIGNAL information item");
                    }
                    self.parser.expect_token(&Token::Eq)?;
                    self.signal_value()?;
                    if !self.parser.consume_token(&Token::Comma) {
                        break;
                    }
                }
            }
            Ok(())
        } else {
            // Delegating only documented body statement families prevents the
            // generic parser from accepting file-scope DECLARE/transaction or
            // unrelated vendor procedural syntax inside a MySQL program.
            if !is_word(
                self.parser,
                &[
                    "SET", "SELECT", "WITH", "INSERT", "REPLACE", "UPDATE", "DELETE", "CALL", "DO",
                ],
            ) {
                return self.expected("a supported MySQL stored-program statement");
            }
            let statement = self.parser.parse_statement()?;
            mysql_expressions::validate(std::slice::from_ref(&statement))
        }
    }
}

fn parameter_type(parser: &mut Parser<'_>) -> Result<(), ParserError> {
    super::parse_mysql_local_type(parser)?;
    if word(parser, "CHARACTER") {
        if !word(parser, "SET") {
            return parser.expected("SET after CHARACTER", parser.peek_token());
        }
        parser.parse_identifier()?;
    }
    if word(parser, "COLLATE") {
        parser.parse_identifier()?;
    }
    Ok(())
}

/// Returns false for non-program statements. Every recognized program must
/// validate to EOF; callers must not ignore a syntax error from this function.
pub(super) fn validate(dialect: &dyn Dialect, sql: &str) -> Result<bool, ParserError> {
    // Most schema statements cannot contain a stored-program body. Avoid a
    // second full tokenization for these obvious native-parser cases. Comments
    // or unusual whitespace simply take the normal token-based path below.
    let mut prefix = sql.split_ascii_whitespace();
    if prefix
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("CREATE"))
        && prefix.next().is_some_and(|word| {
            [
                "TABLE",
                "VIEW",
                "INDEX",
                "UNIQUE",
                "TEMPORARY",
                "DATABASE",
                "SCHEMA",
            ]
            .iter()
            .any(|keyword| word.eq_ignore_ascii_case(keyword))
        })
    {
        return Ok(false);
    }
    let mut parser = if let Some(tokens) = mysql_insert_value_tokens(dialect, sql)? {
        Parser::new(dialect).with_tokens_with_locations(tokens)
    } else {
        Parser::new(dialect).try_with_sql(sql)?
    };
    if !word(&mut parser, "CREATE") {
        return Ok(false);
    }
    let definer = word(&mut parser, "DEFINER");
    if definer {
        parser.expect_token(&Token::Eq)?;
        if word(&mut parser, "CURRENT_USER") {
            if parser.consume_token(&Token::LParen) {
                parser.expect_token(&Token::RParen)?;
            }
        } else {
            match parser.next_token().token {
                Token::Word(_) | Token::SingleQuotedString(_) => {}
                _ => return parser.expected("a DEFINER account", parser.peek_token()),
            }
            if parser.consume_token(&Token::AtSign) {
                match parser.next_token().token {
                    Token::Word(_) | Token::SingleQuotedString(_) => {}
                    _ => return parser.expected("a DEFINER host", parser.peek_token()),
                }
            }
        }
    }
    let trigger = word(&mut parser, "TRIGGER");
    let function = !trigger && word(&mut parser, "FUNCTION");
    let procedure = !trigger && !function && word(&mut parser, "PROCEDURE");
    if !trigger && !function && !procedure {
        return Ok(false);
    }
    if word(&mut parser, "IF") {
        parser.expect_keyword_is(Keyword::NOT)?;
        parser.expect_keyword_is(Keyword::EXISTS)?;
    }
    parser.parse_object_name(false)?;
    let mut parameters = Scope::default();
    if trigger {
        parser.expect_one_of_keywords(&[Keyword::BEFORE, Keyword::AFTER])?;
        parser.expect_one_of_keywords(&[Keyword::INSERT, Keyword::UPDATE, Keyword::DELETE])?;
        parser.expect_keyword_is(Keyword::ON)?;
        parser.parse_object_name(false)?;
        for keyword in [Keyword::FOR, Keyword::EACH, Keyword::ROW] {
            parser.expect_keyword_is(keyword)?;
        }
        if word(&mut parser, "FOLLOWS") || word(&mut parser, "PRECEDES") {
            parser.parse_identifier()?;
        }
    } else {
        parser.expect_token(&Token::LParen)?;
        if !parser.consume_token(&Token::RParen) {
            loop {
                if procedure {
                    let _ =
                        parser.parse_one_of_keywords(&[Keyword::IN, Keyword::OUT, Keyword::INOUT]);
                }
                if !matches!(parser.peek_token().token, Token::Word(ref w) if !w.value.starts_with('@') && !matches!(w.keyword, Keyword::IN | Keyword::OUT | Keyword::INOUT))
                {
                    return parser.expected("a routine parameter name", parser.peek_token());
                }
                let name = parser.parse_identifier()?;
                if !parameters.variables.insert(name.value.to_lowercase()) {
                    return parser.expected("a unique routine parameter", parser.peek_token());
                }
                parameter_type(&mut parser)?;
                if !parser.consume_token(&Token::Comma) {
                    parser.expect_token(&Token::RParen)?;
                    break;
                }
            }
        }
        if function {
            parser.expect_keyword_is(Keyword::RETURNS)?;
            parameter_type(&mut parser)?;
        }
        let mut characteristics = HashSet::new();
        loop {
            let characteristic = if word(&mut parser, "DETERMINISTIC") {
                "determinism"
            } else if word(&mut parser, "NOT") {
                parser.expect_keyword_is(Keyword::DETERMINISTIC)?;
                "determinism"
            } else if word(&mut parser, "LANGUAGE") {
                parser.expect_keyword_is(Keyword::SQL)?;
                "language"
            } else if word(&mut parser, "SQL") {
                parser.expect_keyword_is(Keyword::SECURITY)?;
                parser.expect_one_of_keywords(&[Keyword::DEFINER, Keyword::INVOKER])?;
                "security"
            } else if word(&mut parser, "CONTAINS") || word(&mut parser, "NO") {
                parser.expect_keyword_is(Keyword::SQL)?;
                "data access"
            } else if word(&mut parser, "READS") || word(&mut parser, "MODIFIES") {
                parser.expect_keyword_is(Keyword::SQL)?;
                parser.expect_keyword_is(Keyword::DATA)?;
                "data access"
            } else if word(&mut parser, "COMMENT") {
                if !matches!(parser.next_token().token, Token::SingleQuotedString(_)) {
                    return parser.expected("a quoted routine comment", parser.peek_token());
                }
                "comment"
            } else {
                break;
            };
            if !characteristics.insert(characteristic) {
                return parser
                    .expected("a nonduplicate routine characteristic", parser.peek_token());
            }
        }
    }
    let mut program = Program {
        parser: &mut parser,
        scopes: vec![parameters],
        labels: Vec::new(),
        depth: 0,
        function,
        returns: 0,
    };
    program.statement()?;
    if function && program.returns == 0 {
        return program.expected("RETURN in a stored function");
    }
    if parser.next_token().token != Token::EOF {
        return parser.expected(
            "the end of the stored-program definition",
            parser.peek_token(),
        );
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::MySqlProgramDialect;
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn definer_views_and_events_stay_with_the_native_schema_parser() {
        let dialect = MySqlProgramDialect(MySqlDialect {}, false);
        for sql in [
            "CREATE DEFINER = 'app'@'localhost' VIEW ids AS SELECT id FROM source",
            "CREATE DEFINER = CURRENT_USER() SQL SECURITY INVOKER VIEW ids AS SELECT id FROM source",
            "CREATE DEFINER = 'app'@'localhost' EVENT e ON SCHEDULE EVERY 1 DAY DO SELECT id FROM source",
        ] {
            assert!(!validate(&dialect, sql).unwrap(), "{sql}");
        }
    }

    #[test]
    fn program_errors_keep_original_lines_and_columns() {
        let dialect = MySqlProgramDialect(MySqlDialect {}, false);
        let sql = "CREATE PROCEDURE p()\nBEGIN\n  WHILE @n < 3\n  SET @n = 1;\n  END WHILE;\nEND";
        let error = validate(&dialect, sql).unwrap_err().to_string();
        assert!(error.contains("Expected: DO"), "{error}");
        assert!(error.contains("Line: 4, Column: 3"), "{error}");
    }

    #[test]
    fn excessive_program_nesting_fails_with_a_controlled_error() {
        let dialect = MySqlProgramDialect(MySqlDialect {}, false);
        let sql = format!(
            "CREATE PROCEDURE p() {}SET @n = 1; {}END",
            "BEGIN ".repeat(129),
            "END; ".repeat(128)
        );
        let error = validate(&dialect, &sql).unwrap_err().to_string();
        assert!(error.contains("nesting at most 32 levels"), "{error}");
    }
}
