//! Static mysql-client/session lexical state. This does not execute SQL.
//!
//! https://dev.mysql.com/doc/refman/8.4/en/sql-mode.html
//! https://dev.mysql.com/doc/refman/8.4/en/mysql-commands.html
//! https://github.com/mysql/mysql-server/blob/8.4/client/mysql.cc
use anyhow::{Result, bail};
use sqlparser::{
    dialect::Dialect,
    tokenizer::{Token, Tokenizer},
};
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct MysqlModes {
    no_backslash: bool,
    // Dump save/restore variables, bounded independently of file size. Evicted
    // or overwritten values cannot later be guessed during mode restoration.
    saved: VecDeque<(String, bool)>,
}

/// Argument bytes following the DELIMITER keyword, within the client's
/// 255-byte command buffer. The client stores at most 15 delimiter bytes.
pub(super) fn delimiter(argument: &[u8]) -> Result<Vec<u8>> {
    let start = argument
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(argument.len());
    let argument = &argument[start..];
    let value = match argument.first() {
        Some(quote @ (b'\'' | b'"' | b'`')) => {
            let end = argument[1..]
                .iter()
                .position(|byte| byte == quote)
                .ok_or_else(|| anyhow::anyhow!("unsupported unterminated quoted SQL DELIMITER"))?;
            &argument[1..end + 1]
        }
        // get_arg() uses an ASCII space here, after skipping initial whitespace.
        _ => {
            &argument[..argument
                .iter()
                .position(|byte| *byte == b' ' || *byte == b'\r')
                .unwrap_or(argument.len())]
        }
    };
    if value.is_empty() || value.contains(&0) || value.contains(&b'\\') {
        bail!("invalid SQL DELIMITER");
    }
    Ok(value[..value.len().min(15)].to_vec())
}

fn word(token: &Token) -> Option<&str> {
    match token {
        Token::Word(value) => Some(&value.value),
        _ => None,
    }
}

fn mode_value(value: &str) -> Result<bool> {
    let mut no_backslash = false;
    if value.is_empty() {
        return Ok(false);
    }
    for mode in value.split(',') {
        match mode.trim().to_ascii_uppercase().as_str() {
            "NO_BACKSLASH_ESCAPES" => no_backslash = true,
            // These modes change validation/runtime behavior without changing
            // the declaration and reference syntax extracted by this module.
            "ALLOW_INVALID_DATES"
            | "ERROR_FOR_DIVISION_BY_ZERO"
            | "NO_AUTO_VALUE_ON_ZERO"
            | "NO_DIR_IN_CREATE"
            | "NO_ENGINE_SUBSTITUTION"
            | "NO_ZERO_DATE"
            | "NO_ZERO_IN_DATE"
            | "ONLY_FULL_GROUP_BY"
            | "STRICT_ALL_TABLES"
            | "STRICT_TRANS_TABLES"
            | "TIME_TRUNCATE_FRACTIONAL"
            | "TRADITIONAL" => {}
            other => bail!("unsupported MySQL sql_mode {other:?}; normalize the dump first"),
        }
    }
    Ok(no_backslash)
}

/// Static session state for literal mode assignments and known dump snapshots.
/// Unknown mode expressions fail explicitly rather than guessing boundaries.
impl MysqlModes {
    pub(super) fn no_backslash_escapes(&self) -> bool {
        self.no_backslash
    }

    pub(super) fn invalidate_saved(&mut self) {
        self.saved.clear();
    }

    fn invalidate_expression_assignments(&mut self, tokens: &[Token]) {
        for pair in tokens.windows(2) {
            if pair[1] == Token::Assignment {
                if let Some(name) = word(&pair[0]) {
                    self.saved
                        .retain(|(saved, _)| !saved.eq_ignore_ascii_case(name));
                } else {
                    // Quoted user-variable names are separate tokens. Their
                    // values are outside the supported snapshot grammar.
                    self.saved.clear();
                }
            }
        }
    }

    pub(super) fn apply(&mut self, sql: &str, dialect: &dyn Dialect) -> Result<()> {
        let keyword = sql.split_whitespace().next().unwrap_or("");
        if keyword.eq_ignore_ascii_case("CALL") {
            // Routine execution is not interpreted interprocedurally. A CALL
            // can mutate a user variable saved by an earlier dump header.
            self.saved.clear();
            return Ok(());
        }
        let set = keyword.eq_ignore_ascii_case("SET");
        if !set && (self.saved.is_empty() || !sql.contains('@')) {
            return Ok(());
        }
        let tokens: Vec<_> = Tokenizer::new(dialect, sql)
            .tokenize()?
            .into_iter()
            .filter(|token| !matches!(token, Token::Whitespace(_)))
            .collect();
        if keyword.eq_ignore_ascii_case("CREATE") {
            let object_kind = tokens.iter().find_map(|token| match token {
                Token::Word(word) if word.quote_style.is_none() => {
                    let value = word.value.to_ascii_uppercase();
                    matches!(
                        value.as_str(),
                        "PROCEDURE"
                            | "FUNCTION"
                            | "TRIGGER"
                            | "EVENT"
                            | "TABLE"
                            | "VIEW"
                            | "SCHEMA"
                            | "DATABASE"
                            | "INDEX"
                            | "USER"
                            | "ROLE"
                    )
                    .then_some(value)
                }
                _ => None,
            });
            if object_kind.is_some_and(|kind| {
                matches!(
                    kind.as_str(),
                    "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"
                )
            }) {
                // Defining a stored body does not execute its assignments.
                return Ok(());
            }
        }
        if !set {
            self.invalidate_expression_assignments(&tokens);
            // SELECT ... INTO @var also overwrites a known saved value.
            for (index, token) in tokens.iter().enumerate() {
                if word(token).is_some_and(|value| value.eq_ignore_ascii_case("INTO")) {
                    for token in &tokens[index + 1..] {
                        if *token == Token::AtSign {
                            self.saved.clear();
                        }
                        if let Some(name) = word(token).filter(|name| name.starts_with('@')) {
                            if name == "@" {
                                self.saved.clear();
                            } else {
                                self.saved
                                    .retain(|(saved, _)| !saved.eq_ignore_ascii_case(name));
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        let mut begin = 1;
        let mut nesting = 0usize;
        let mut default_scope = "";
        for end in 1..=tokens.len() {
            let boundary = end == tokens.len()
                || (nesting == 0 && matches!(tokens[end], Token::Comma | Token::SemiColon));
            if boundary {
                let assignment = &tokens[begin..end];
                self.invalidate_expression_assignments(assignment);
                let equals = assignment.iter().position(|token| *token == Token::Eq);
                let lhs = &assignment[..equals.unwrap_or(assignment.len())];
                let mut scope = default_scope;
                let lhs = if lhs.first().and_then(word).is_some_and(|value| {
                    matches!(
                        value.to_ascii_uppercase().as_str(),
                        "SESSION" | "LOCAL" | "GLOBAL" | "PERSIST" | "PERSIST_ONLY"
                    )
                }) {
                    scope = word(&lhs[0]).unwrap();
                    default_scope = scope;
                    &lhs[1..]
                } else {
                    lhs
                };
                let name = match lhs {
                    [Token::Word(name)] => name.value.to_ascii_lowercase(),
                    [Token::Word(scope), Token::Period, Token::Word(name)] => {
                        format!("{}.{}", scope.value, name.value).to_ascii_lowercase()
                    }
                    _ => String::new(),
                };
                let target = matches!(
                    name.as_str(),
                    "sql_mode"
                        | "@@sql_mode"
                        | "@@session.sql_mode"
                        | "@@local.sql_mode"
                        | "@@global.sql_mode"
                        | "@@persist.sql_mode"
                        | "@@persist_only.sql_mode"
                );
                if name.starts_with('@') && !name.starts_with("@@") && lhs.len() == 1 {
                    self.saved.retain(|(saved, _)| saved != &name);
                    let value = equals.and_then(|index| match &assignment[index + 1..] {
                        [Token::Word(value)] if value.value.eq_ignore_ascii_case("@@sql_mode") => {
                            Some(self.no_backslash)
                        }
                        [Token::Word(scope), Token::Period, Token::Word(value)]
                            if matches!(
                                scope.value.to_ascii_lowercase().as_str(),
                                "@@session" | "@@local"
                            ) && value.value.eq_ignore_ascii_case("sql_mode") =>
                        {
                            Some(self.no_backslash)
                        }
                        _ => None,
                    });
                    if let Some(value) = value.filter(|_| name.len() <= 256) {
                        if self.saved.len() == 128 {
                            self.saved.pop_front();
                        }
                        self.saved.push_back((name, value));
                    }
                } else if target {
                    let values = equals.map(|index| &assignment[index + 1..]);
                    let value = match values {
                    Some([Token::SingleQuotedString(value) | Token::DoubleQuotedString(value)]) => {
                        mode_value(value)?
                    }
                    Some([Token::Word(value)]) if value.value.starts_with('@') && !value.value.starts_with("@@") => {
                        self.saved.iter().find(|(name, _)| name.eq_ignore_ascii_case(&value.value))
                            .map(|(_, value)| *value)
                            .ok_or_else(|| anyhow::anyhow!("unsupported MySQL sql_mode expression; saved mode variable is unknown or overwritten"))?
                    }
                    _ => bail!(
                        "unsupported MySQL sql_mode expression; expected one literal mode string"
                    ),
                };
                    let global = if name.starts_with("@@") {
                        name.starts_with("@@global.")
                            || name.starts_with("@@persist.")
                            || name.starts_with("@@persist_only.")
                    } else {
                        matches!(
                            scope.to_ascii_uppercase().as_str(),
                            "GLOBAL" | "PERSIST" | "PERSIST_ONLY"
                        )
                    };
                    if !global {
                        self.no_backslash = value;
                    }
                } else if lhs.iter().any(|token| {
                    *token == Token::AtSign
                        || word(token)
                            .is_some_and(|name| name.starts_with('@') && !name.starts_with("@@"))
                }) {
                    self.saved.clear();
                } else if lhs.iter().any(|token| {
                    word(token).is_some_and(|value| {
                        value.eq_ignore_ascii_case("sql_mode")
                            || value.eq_ignore_ascii_case("@@sql_mode")
                    })
                }) {
                    bail!("unsupported MySQL sql_mode assignment target");
                }
                begin = end + 1;
            } else {
                match tokens[end] {
                    Token::LParen => nesting += 1,
                    Token::RParen => nesting = nesting.saturating_sub(1),
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    fn statement_mode(sql: &str, dialect: &dyn Dialect) -> Result<Option<bool>> {
        let mut modes = MysqlModes::default();
        modes.apply(sql, dialect)?;
        Ok(modes.no_backslash.then_some(true))
    }

    #[test]
    fn literal_modes_and_only_actual_assignment_targets_change_state() {
        for sql in [
            "SET sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET SESSION sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET LOCAL `sql_mode`='NO_BACKSLASH_ESCAPES'",
            "SET @@SESSION.sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET @@sql_mode='NO_BACKSLASH_ESCAPES', autocommit=1",
            "SET autocommit=1, sql_mode='NO_BACKSLASH_ESCAPES'",
        ] {
            assert_eq!(
                statement_mode(sql, &MySqlDialect {}).unwrap(),
                Some(true),
                "{sql}"
            );
        }
        for sql in [
            "SET @sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET @old_sql_mode=@@sql_mode",
            "SET @note='sql_mode=NO_BACKSLASH_ESCAPES'",
            "SET GLOBAL sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET @@GLOBAL.sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET GLOBAL sort_buffer_size=10000, sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET PERSIST sql_mode='NO_BACKSLASH_ESCAPES'",
        ] {
            assert_eq!(
                statement_mode(sql, &MySqlDialect {}).unwrap(),
                None,
                "{sql}"
            );
        }
        assert_eq!(
            statement_mode("SET sql_mode=''", &MySqlDialect {}).unwrap(),
            None
        );
        assert_eq!(
            statement_mode(
                "SET sql_mode='STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES'",
                &MySqlDialect {}
            )
            .unwrap(),
            Some(true)
        );
    }

    #[test]
    fn known_dump_mode_saves_restore_and_overwrites_invalidate() {
        let dialect = MySqlDialect {};
        let mut modes = MysqlModes::default();
        modes.apply("SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO,NO_BACKSLASH_ESCAPES'", &dialect).unwrap();
        assert!(modes.no_backslash_escapes());
        modes.apply("SET SQL_MODE=@OLD_SQL_MODE", &dialect).unwrap();
        assert!(!modes.no_backslash_escapes());
        modes
            .apply(
                "CREATE PROCEDURE dormant() SELECT @OLD_SQL_MODE := 'ANSI_QUOTES'",
                &dialect,
            )
            .unwrap();
        modes.apply("SET SQL_MODE=@OLD_SQL_MODE", &dialect).unwrap();
        modes.apply("SET @OLD_SQL_MODE=123", &dialect).unwrap();
        assert!(modes.apply("SET SQL_MODE=@OLD_SQL_MODE", &dialect).is_err());
        for overwrite in [
            "SELECT @OLD_SQL_MODE := 'ANSI_QUOTES'",
            "SELECT 'ANSI_QUOTES' INTO @OLD_SQL_MODE",
            "SET @dummy=(@OLD_SQL_MODE := 'ANSI_QUOTES')",
            "SET @'OLD_SQL_MODE'='ANSI_QUOTES'",
            "SET @\"OLD_SQL_MODE\"='ANSI_QUOTES'",
            "SET @`OLD_SQL_MODE`='ANSI_QUOTES'",
            "SELECT @'OLD_SQL_MODE' := 'ANSI_QUOTES'",
            "SELECT 'ANSI_QUOTES' INTO @'OLD_SQL_MODE'",
            "CALL change_saved()",
        ] {
            modes
                .apply("SET @OLD_SQL_MODE=@@SQL_MODE", &dialect)
                .unwrap();
            modes.apply(overwrite, &dialect).unwrap();
            assert!(
                modes.apply("SET SQL_MODE=@OLD_SQL_MODE", &dialect).is_err(),
                "{overwrite}"
            );
        }
    }

    #[test]
    fn unknown_modes_expressions_and_malformed_assignments_fail_closed() {
        for sql in [
            "SET sql_mode=DEFAULT",
            "SET sql_mode=@old_mode",
            "SET sql_mode=CONCAT(@@sql_mode,',NO_BACKSLASH_ESCAPES')",
            "SET sql_mode='ANSI_QUOTES'",
            "SET sql_mode='PIPES_AS_CONCAT'",
            "SET sql_mode='ANSI'",
            "SET sql_mode='FUTURE_MODE'",
            "SET sql_mode 'NO_BACKSLASH_ESCAPES'",
            "SET sql_mode='NO_BACKSLASH_ESCAPES' trailing",
            "SET sql_mode=NO_BACKSLASH_ESCAPES",
        ] {
            assert!(statement_mode(sql, &MySqlDialect {}).is_err(), "{sql}");
        }
    }

    #[test]
    fn mysql_client_delimiter_arguments() {
        for (input, expected) in [
            (" too trailing_text", "too"),
            (" abcdefghijklmnopq", "abcdefghijklmno"),
            (" '$ $' trailing", "$ $"),
            (" \"//\"", "//"),
            (" `;;`", ";;"),
        ] {
            assert_eq!(delimiter(input.as_bytes()).unwrap(), expected.as_bytes());
        }
        for input in ["", " ", " ''", " 'unfinished", " '\\x'", " \\x"] {
            assert!(delimiter(input.as_bytes()).is_err(), "{input}");
        }
    }
}
