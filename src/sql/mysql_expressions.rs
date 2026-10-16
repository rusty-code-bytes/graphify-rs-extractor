//! Narrow MySQL expression grammar additions for sqlparser 0.59.
//!
//! No tokens or source ranges are erased. Every expression is parsed normally,
//! retaining nested query ASTs and identifier spans for dependency extraction.
//! sqlparser has no CHAR charset or JSON_VALUE response-clause node, so those
//! clauses use named function arguments as an internal graph projection. This
//! AST is not a SQL pretty-printer or an expression evaluator.

use sqlparser::{
    ast::{
        BinaryOperator, CastKind, DataType, DateTimeField, Expr, ExtractSyntax, Function,
        FunctionArg, FunctionArgExpr, FunctionArgOperator, FunctionArgumentClause,
        FunctionArgumentList, FunctionArguments, Ident, Interval, JsonReturningClause, ObjectName,
        Statement, UnaryOperator, Value, Visit, Visitor,
    },
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::Token,
};
use std::ops::ControlFlow;

// Relative to sqlparser's multiplication=40, addition=30, &=23 and |=21.
// MySQL gives ^ higher precedence than multiplication and shifts lower than +.
const BIT_XOR: u8 = 45;
const SHIFT: u8 = 25;
const UNARY: u8 = 46;

pub(super) fn get_next_precedence(parser: &Parser<'_>) -> Option<Result<u8, ParserError>> {
    match parser.peek_token().token {
        Token::Word(word) if word.keyword == Keyword::MOD => Some(Ok(40)),
        Token::ShiftLeft | Token::ShiftRight => Some(Ok(SHIFT)),
        Token::Caret => Some(Ok(BIT_XOR)),
        // A bare ~ is unary in MySQL, never the PostgreSQL regexp infix.
        Token::Tilde => Some(Ok(0)),
        _ => None,
    }
}

pub(super) fn parse_infix(
    parser: &mut Parser<'_>,
    expr: &Expr,
    precedence: u8,
) -> Option<Result<Expr, ParserError>> {
    let operator = match parser.peek_token().token {
        Token::Word(word) if word.keyword == Keyword::MOD => BinaryOperator::Modulo,
        // The upstream MySQL hook unwraps a parse error and parses the whole
        // right expression; use the Pratt binding power and propagate errors.
        Token::Word(word) if word.keyword == Keyword::DIV => BinaryOperator::MyIntegerDivide,
        Token::ShiftLeft => BinaryOperator::PGBitwiseShiftLeft,
        Token::ShiftRight => BinaryOperator::PGBitwiseShiftRight,
        _ => return None,
    };
    parser.next_token();
    Some(
        parser
            .parse_subexpr(precedence)
            .map(|right| Expr::BinaryOp {
                left: Box::new(expr.clone()),
                op: operator,
                right: Box::new(right),
            }),
    )
}

pub(super) fn parse_prefix(parser: &mut Parser<'_>) -> Option<Result<Expr, ParserError>> {
    match parser.peek_token().token {
        Token::Tilde => {
            parser.next_token();
            Some(parser.parse_subexpr(UNARY).map(|expr| Expr::UnaryOp {
                op: UnaryOperator::PGBitwiseNot,
                expr: Box::new(expr),
            }))
        }
        // Unary +/- share ~'s binding power in the default MySQL SQL mode.
        token @ (Token::Plus | Token::Minus) => {
            parser.next_token();
            Some(parser.parse_subexpr(UNARY).map(|expr| Expr::UnaryOp {
                op: if token == Token::Plus {
                    UnaryOperator::Plus
                } else {
                    UnaryOperator::Minus
                },
                expr: Box::new(expr),
            }))
        }
        Token::Word(word) if word.quote_style.is_none() => match word.keyword {
            Keyword::INTERVAL => {
                let name = parser.parse_identifier();
                Some(name.and_then(|name| parse_interval(parser, name)))
            }
            Keyword::EXTRACT if parser.peek_nth_token(1).token == Token::LParen => {
                parser.next_token();
                Some(parse_extract(parser))
            }
            Keyword::CHAR if parser.peek_nth_token(1).token == Token::LParen => {
                let name = parser.parse_identifier();
                Some(name.and_then(|name| parse_char(parser, name)))
            }
            Keyword::CAST if parser.peek_nth_token(1).token == Token::LParen => {
                parser.next_token();
                Some(parse_cast(parser))
            }
            _ if word.value.eq_ignore_ascii_case("JSON_VALUE")
                && parser.peek_nth_token(1).token == Token::LParen =>
            {
                let name = parser.parse_identifier();
                Some(name.and_then(|name| parse_json_value(parser, name)))
            }
            _ => None,
        },
        _ => None,
    }
}

fn function(name: Ident, args: Vec<FunctionArg>, clauses: Vec<FunctionArgumentClause>) -> Expr {
    Expr::Function(Function {
        name: ObjectName::from(vec![name]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args,
            clauses,
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

fn argument(expr: Expr) -> FunctionArg {
    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
}

fn named_argument(name: Ident, expr: Expr) -> FunctionArg {
    FunctionArg::Named {
        name,
        arg: FunctionArgExpr::Expr(expr),
        operator: FunctionArgOperator::Equals,
    }
}

fn parse_char(parser: &mut Parser<'_>, name: Ident) -> Result<Expr, ParserError> {
    parser.expect_token(&Token::LParen)?;
    let mut args = parser.parse_comma_separated(|p| p.parse_expr().map(argument))?;
    if parser.peek_keyword(Keyword::USING) {
        let clause = parser.parse_identifier()?;
        let charset = parse_charset(parser)?;
        args.push(named_argument(clause, Expr::Identifier(charset)));
    }
    parser.expect_token(&Token::RParen)?;
    Ok(function(name, args, vec![]))
}

fn parse_charset(parser: &mut Parser<'_>) -> Result<Ident, ParserError> {
    // A charset is one name, never an expression, qualified relation, or query.
    let name = match parser.peek_token().token {
        Token::Word(word) => {
            word.keyword == Keyword::NoKeyword
                || word.quote_style.is_some()
                || word.keyword == Keyword::BINARY
        }
        Token::SingleQuotedString(_) | Token::DoubleQuotedString(_) => true,
        _ => false,
    };
    if !name {
        return parser.expected("a MySQL character set name", parser.peek_token());
    }
    let span = parser.peek_token().span;
    let mut charset = parser.parse_identifier()?;
    charset.span = span;
    Ok(charset)
}

fn temporal_unit(parser: &mut Parser<'_>) -> Result<DateTimeField, ParserError> {
    if let Token::Word(word) = parser.peek_token().token
        && word.quote_style.is_none()
    {
        let upper = word.value.to_ascii_uppercase();
        if matches!(
            upper.as_str(),
            "SECOND_MICROSECOND"
                | "MINUTE_MICROSECOND"
                | "MINUTE_SECOND"
                | "HOUR_MICROSECOND"
                | "HOUR_SECOND"
                | "HOUR_MINUTE"
                | "DAY_MICROSECOND"
                | "DAY_SECOND"
                | "DAY_MINUTE"
                | "DAY_HOUR"
                | "YEAR_MONTH"
        ) {
            return parser.parse_identifier().map(DateTimeField::Custom);
        }
        if matches!(
            word.keyword,
            Keyword::MICROSECOND
                | Keyword::SECOND
                | Keyword::MINUTE
                | Keyword::HOUR
                | Keyword::DAY
                | Keyword::WEEK
                | Keyword::MONTH
                | Keyword::QUARTER
                | Keyword::YEAR
        ) {
            return parser.parse_date_time_field();
        }
    }
    parser.expected("a MySQL temporal interval unit", parser.peek_token())
}

fn parse_interval(parser: &mut Parser<'_>, name: Ident) -> Result<Expr, ParserError> {
    // INTERVAL(expr, threshold, ...) and INTERVAL (expr) unit share a prefix.
    // Speculate only on the complete function grammar and rewind on failure.
    if parser.peek_token().token == Token::LParen
        && let Some(args) = parser.maybe_parse(|p| {
            p.expect_token(&Token::LParen)?;
            let args = p.parse_comma_separated(|p| p.parse_expr().map(argument))?;
            p.expect_token(&Token::RParen)?;
            if args.len() < 2 {
                return p.expected("at least two INTERVAL function arguments", p.peek_token());
            }
            Ok(args)
        })?
    {
        return Ok(function(name, args, vec![]));
    }
    let value = parser.parse_expr()?;
    let unit = temporal_unit(parser)?;
    Ok(Expr::Interval(Interval {
        value: Box::new(value),
        leading_field: Some(unit),
        leading_precision: None,
        last_field: None,
        fractional_seconds_precision: None,
    }))
}

fn parse_extract(parser: &mut Parser<'_>) -> Result<Expr, ParserError> {
    parser.expect_token(&Token::LParen)?;
    let field = temporal_unit(parser)?;
    parser.expect_keyword_is(Keyword::FROM)?;
    let expr = parser.parse_expr()?;
    parser.expect_token(&Token::RParen)?;
    Ok(Expr::Extract {
        field,
        syntax: ExtractSyntax::From,
        expr: Box::new(expr),
    })
}

fn parse_char_type_charset(
    parser: &mut Parser<'_>,
    data_type: &DataType,
) -> Result<Option<Ident>, ParserError> {
    if !matches!(data_type, DataType::Char(_) | DataType::Character(_)) {
        return Ok(None);
    }
    if parser.parse_keywords(&[Keyword::CHARACTER, Keyword::SET]) {
        return parse_charset(parser).map(Some);
    }
    // MySQL's documented charset shorthands. Keep source spans on the name.
    if let Token::Word(word) = parser.peek_token().token
        && word.quote_style.is_none()
        && (word.value.eq_ignore_ascii_case("ASCII") || word.value.eq_ignore_ascii_case("UNICODE"))
    {
        let mut charset = parser.parse_identifier()?;
        charset.value = if word.value.eq_ignore_ascii_case("ASCII") {
            "latin1"
        } else {
            "ucs2"
        }
        .into();
        return Ok(Some(charset));
    }
    Ok(None)
}

fn parse_cast(parser: &mut Parser<'_>) -> Result<Expr, ParserError> {
    parser.expect_token(&Token::LParen)?;
    let expr = parser.parse_expr()?;
    parser.expect_keyword_is(Keyword::AS)?;
    let data_type = parser.parse_data_type()?;
    let charset = parse_char_type_charset(parser, &data_type)?;
    let format = parser.parse_optional_cast_format()?;
    parser.expect_token(&Token::RParen)?;
    if let Some(charset) = charset {
        if format.is_some() {
            return parser.expected("a MySQL CAST without FORMAT", parser.peek_token());
        }
        // CAST(expr AS CHAR ... CHARACTER SET name) is documented as the
        // equivalent CONVERT(expr, CHAR ... CHARACTER SET name).
        Ok(Expr::Convert {
            is_try: false,
            expr: Box::new(expr),
            data_type: Some(data_type),
            charset: Some(ObjectName::from(vec![charset])),
            target_before_value: false,
            styles: vec![],
        })
    } else {
        Ok(Expr::Cast {
            kind: CastKind::Cast,
            expr: Box::new(expr),
            data_type,
            format,
        })
    }
}

fn parse_json_value(parser: &mut Parser<'_>, name: Ident) -> Result<Expr, ParserError> {
    parser.expect_token(&Token::LParen)?;
    let document = parser.parse_expr()?;
    parser.expect_token(&Token::Comma)?;
    if !matches!(
        parser.peek_token().token,
        Token::SingleQuotedString(_) | Token::DoubleQuotedString(_)
    ) {
        return parser.expected("a JSON_VALUE path string literal", parser.peek_token());
    }
    let path = Expr::Value(parser.parse_value()?);
    let mut args = vec![argument(document), argument(path)];
    let mut clauses = vec![];
    if parser.parse_keyword(Keyword::RETURNING) {
        if !matches!(parser.peek_token().token, Token::Word(ref word)
            if matches!(word.keyword, Keyword::FLOAT | Keyword::DOUBLE | Keyword::DECIMAL
                | Keyword::SIGNED | Keyword::UNSIGNED | Keyword::DATE | Keyword::TIME
                | Keyword::DATETIME | Keyword::YEAR | Keyword::CHAR | Keyword::JSON))
        {
            return parser.expected("a MySQL JSON_VALUE return type", parser.peek_token());
        }
        let data_type = parser.parse_data_type()?;
        if let Some(charset) = parse_char_type_charset(parser, &data_type)? {
            args.push(named_argument(
                Ident::with_span(charset.span, "CHARACTER SET"),
                Expr::Identifier(charset),
            ));
        }
        clauses.push(FunctionArgumentClause::JsonReturningClause(
            JsonReturningClause { data_type },
        ));
    }
    let mut seen_empty = false;
    let mut seen_error = false;
    while parser
        .peek_one_of_keywords(&[Keyword::NULL, Keyword::ERROR, Keyword::DEFAULT])
        .is_some()
    {
        let response = parser.parse_identifier()?;
        let value = if response.value.eq_ignore_ascii_case("DEFAULT") {
            parse_signed_literal(parser)?
        } else if response.value.eq_ignore_ascii_case("NULL") {
            Expr::Value(Value::Null.with_span(response.span))
        } else {
            Expr::Identifier(response.clone())
        };
        parser.expect_keyword_is(Keyword::ON)?;
        let condition = parser.expect_one_of_keywords(&[Keyword::EMPTY, Keyword::ERROR])?;
        if condition == Keyword::EMPTY {
            if seen_empty || seen_error {
                return parser.expected("ON EMPTY once and before ON ERROR", parser.peek_token());
            }
            seen_empty = true;
        } else {
            if seen_error {
                return parser.expected("ON ERROR once", parser.peek_token());
            }
            seen_error = true;
        }
        let mut clause = response;
        clause.value.push_str(if condition == Keyword::EMPTY {
            " ON EMPTY"
        } else {
            " ON ERROR"
        });
        args.push(named_argument(clause, value));
    }
    parser.expect_token(&Token::RParen)?;
    Ok(function(name, args, clauses))
}

fn parse_signed_literal(parser: &mut Parser<'_>) -> Result<Expr, ParserError> {
    // MySQL's json_on_response grammar uses signed_literal, which excludes
    // identifiers, queries, expressions and NULL. NULL ON EMPTY is a separate
    // response clause; DEFAULT NULL ON EMPTY is a syntax error.
    let sign = match parser.peek_token().token {
        Token::Plus => Some(UnaryOperator::Plus),
        Token::Minus => Some(UnaryOperator::Minus),
        _ => None,
    };
    if let Some(op) = sign {
        parser.next_token();
        if !matches!(parser.peek_token().token, Token::Number(..)) {
            return parser.expected("a signed numeric literal", parser.peek_token());
        }
        return parser.parse_value().map(|value| Expr::UnaryOp {
            op,
            expr: Box::new(Expr::Value(value)),
        });
    }
    let literal = match parser.peek_token().token {
        Token::Number(..)
        | Token::SingleQuotedString(_)
        | Token::DoubleQuotedString(_)
        | Token::HexStringLiteral(_)
        | Token::NationalStringLiteral(_) => true,
        Token::Word(word) => matches!(word.keyword, Keyword::TRUE | Keyword::FALSE),
        _ => false,
    };
    if !literal {
        return parser.expected("a JSON_VALUE DEFAULT signed literal", parser.peek_token());
    }
    parser.parse_value().map(Expr::Value)
}

// The unmodified parser can consume AS as an unquoted right-hand identifier
// (`value <=> AS result`). Run this check on any native-parser success too.
struct ReservedOperand;
impl Visitor for ReservedOperand {
    type Break = ParserError;
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        let first = match expr {
            Expr::Identifier(ident) => Some(ident),
            Expr::CompoundIdentifier(parts) => parts.first(),
            _ => None,
        };
        // Reserved words are legal after a period, but not as the unquoted
        // leading identifier: t.AS is valid, AS.t is not.
        if let Some(ident) = first
            && ident.quote_style.is_none()
            && ident.value.eq_ignore_ascii_case("AS")
        {
            return ControlFlow::Break(ParserError::ParserError(format!(
                "Expected an expression before AS at Line: {}, Column: {}",
                ident.span.start.line, ident.span.start.column
            )));
        }
        ControlFlow::Continue(())
    }
}

pub(super) fn validate(statements: &[Statement]) -> Result<(), ParserError> {
    for statement in statements {
        if let ControlFlow::Break(error) = statement.visit(&mut ReservedOperand) {
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn validate_expression(expr: &Expr) -> Result<(), ParserError> {
    if let ControlFlow::Break(error) = expr.visit(&mut ReservedOperand) {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::MySqlProgramDialect;
    use sqlparser::dialect::MySqlDialect;

    fn expression(sql: &str) -> Expr {
        let dialect = MySqlProgramDialect(MySqlDialect {}, false);
        let mut parser = Parser::new(&dialect).try_with_sql(sql).unwrap();
        let expr = parser.parse_expr().unwrap();
        assert_eq!(parser.peek_token().token, Token::EOF, "{sql}");
        expr
    }

    fn binary(expr: Expr, expected: BinaryOperator) -> (Expr, Expr) {
        let Expr::BinaryOp { left, op, right } = expr else {
            panic!("expected {expected}, got {expr:?}");
        };
        assert_eq!(op, expected);
        (*left, *right)
    }

    #[test]
    fn mysql_operators_keep_binding_power_and_associativity() {
        let (left, right) = binary(expression("8 DIV 2 + 1"), BinaryOperator::Plus);
        binary(left, BinaryOperator::MyIntegerDivide);
        assert_eq!(right.to_string(), "1");
        let (left, right) = binary(expression("8 MOD 3 MOD 2"), BinaryOperator::Modulo);
        binary(left, BinaryOperator::Modulo);
        assert_eq!(right.to_string(), "2");
        let (left, right) = binary(
            expression("1 + 2 << 3 + 4"),
            BinaryOperator::PGBitwiseShiftLeft,
        );
        binary(left, BinaryOperator::Plus);
        binary(right, BinaryOperator::Plus);
        let (left, right) = binary(expression("8 >> 2 & 1"), BinaryOperator::BitwiseAnd);
        binary(left, BinaryOperator::PGBitwiseShiftRight);
        assert_eq!(right.to_string(), "1");
        let (left, _) = binary(expression("~1 ^ 2 * 3"), BinaryOperator::Multiply);
        let (left, _) = binary(left, BinaryOperator::BitwiseXor);
        assert!(matches!(
            left,
            Expr::UnaryOp {
                op: UnaryOperator::PGBitwiseNot,
                ..
            }
        ));
        let (left, _) = binary(expression("-1 ^ 2"), BinaryOperator::BitwiseXor);
        assert!(matches!(
            left,
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                ..
            }
        ));
    }

    #[test]
    fn mysql_composite_temporal_units_are_finite_and_keep_their_spans() {
        for unit in [
            "SECOND_MICROSECOND",
            "MINUTE_MICROSECOND",
            "MINUTE_SECOND",
            "HOUR_MICROSECOND",
            "HOUR_SECOND",
            "HOUR_MINUTE",
            "DAY_MICROSECOND",
            "DAY_SECOND",
            "DAY_MINUTE",
            "DAY_HOUR",
            "YEAR_MONTH",
        ] {
            let Expr::Interval(interval) = expression(&format!("INTERVAL '1'\n{unit}")) else {
                panic!("missing interval");
            };
            let Some(DateTimeField::Custom(ident)) = interval.leading_field else {
                panic!("missing composite unit {unit}");
            };
            assert_eq!(ident.value, unit);
            assert_eq!(ident.span.start.line, 2);
            let Expr::Extract {
                field: DateTimeField::Custom(ident),
                ..
            } = expression(&format!("EXTRACT(\n{unit} FROM d)"))
            else {
                panic!("missing EXTRACT field {unit}");
            };
            assert_eq!(ident.value, unit);
            assert_eq!(ident.span.start.line, 2);
        }
    }

    #[test]
    fn mysql_interval_function_and_parenthesized_temporal_value_are_distinct() {
        assert!(
            matches!(expression("INTERVAL(1, 2, 3) + 4"), Expr::BinaryOp { left, .. }
            if matches!(*left, Expr::Function(_)))
        );
        assert!(matches!(
            expression("INTERVAL (1 + 2) DAY"),
            Expr::Interval(_)
        ));
        assert!(
            matches!(expression("INTERVAL (SELECT n FROM source) DAY"), Expr::Interval(Interval { value, .. })
            if matches!(*value, Expr::Subquery(_)))
        );
    }

    #[test]
    fn mysql_json_value_preserves_literal_defaults_and_return_type() {
        let Expr::Function(call) = expression(
            "JSON_VALUE(j, '$.x' RETURNING DECIMAL(8, 2) DEFAULT -1 ON EMPTY DEFAULT 2 ON ERROR)",
        ) else {
            panic!("missing JSON_VALUE");
        };
        let FunctionArguments::List(args) = call.args else {
            panic!("missing arguments")
        };
        assert_eq!(args.args.len(), 4);
        assert!(
            matches!(&args.args[2], FunctionArg::Named { name, arg: FunctionArgExpr::Expr(Expr::UnaryOp { op: UnaryOperator::Minus, .. }), .. }
            if name.value == "DEFAULT ON EMPTY")
        );
        assert!(
            matches!(&args.args[3], FunctionArg::Named { name, arg: FunctionArgExpr::Expr(Expr::Value(_)), .. }
            if name.value == "DEFAULT ON ERROR")
        );
        assert!(matches!(
            &args.clauses[0],
            FunctionArgumentClause::JsonReturningClause(_)
        ));
    }
}
