//! A small arithmetic evaluator behind the `=` prefix.
//!
//! Recursive descent rather than shunting-yard, which gets correct unary minus
//! and right-associative `^` for free. The evaluator is deliberately *total*:
//! there is no divide-by-zero error, because `1/0` is simply infinity and
//! [`format_result`] renders it. Fewer error paths, no surprises.

use std::f64::consts;

#[derive(Clone, Debug, PartialEq)]
pub enum CalcError {
    UnexpectedChar {
        index: usize,
        ch: char,
    },
    UnexpectedEnd,
    UnknownIdent(String),
    BadArity {
        name: String,
        expected: usize,
        got: usize,
    },
    Trailing {
        index: usize,
    },
}

impl std::fmt::Display for CalcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedChar { index, ch } => {
                write!(formatter, "unexpected '{ch}' at position {}", index + 1)
            }
            Self::UnexpectedEnd => write!(formatter, "expression is incomplete"),
            Self::UnknownIdent(name) => write!(formatter, "unknown name '{name}'"),
            Self::BadArity {
                name,
                expected,
                got,
            } => {
                write!(formatter, "{name} takes {expected} argument(s), got {got}")
            }
            Self::Trailing { index } => {
                write!(formatter, "unexpected input at position {}", index + 1)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
    Comma,
}

/// Evaluates an arithmetic expression.
pub fn eval(input: &str) -> Result<f64, CalcError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(CalcError::UnexpectedEnd);
    }

    let mut parser = Parser {
        tokens,
        position: 0,
    };
    let value = parser.expression()?;
    if parser.position < parser.tokens.len() {
        return Err(CalcError::Trailing {
            index: parser.position,
        });
    }

    Ok(value)
}

/// Renders a result for display, keeping integers clean and naming non-finite values.
pub fn format_result(value: f64) -> String {
    if value.is_nan() {
        return "undefined".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-∞"
        } else {
            "∞"
        }
        .to_string();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{value:.0}");
    }

    let mut text = format!("{value:.10}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }

    if text.parse::<f64>().is_ok_and(|parsed| parsed == 0.0) && value != 0.0 {
        // Values too small for fixed notation would render as "0"; keep precision.
        return format!("{value:e}");
    }

    text
}

fn tokenize(input: &str) -> Result<Vec<Token>, CalcError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];

        if ch.is_whitespace() {
            index += 1;
            continue;
        }

        if ch.is_ascii_digit() || ch == '.' {
            let (value, next) = read_number(&chars, index)?;
            tokens.push(Token::Number(value));
            index = next;
            continue;
        }

        if ch.is_alphabetic() || ch == '_' {
            let start = index;
            while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
                index += 1;
            }
            tokens.push(Token::Ident(chars[start..index].iter().collect()));
            continue;
        }

        let token = match ch {
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' => Token::Star,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '^' => Token::Caret,
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            _ => return Err(CalcError::UnexpectedChar { index, ch }),
        };
        tokens.push(token);
        index += 1;
    }

    Ok(tokens)
}

fn read_number(chars: &[char], start: usize) -> Result<(f64, usize), CalcError> {
    if chars[start] == '0'
        && start + 1 < chars.len()
        && let Some(radix) = match chars[start + 1] {
            'x' | 'X' => Some(16),
            'b' | 'B' => Some(2),
            'o' | 'O' => Some(8),
            _ => None,
        }
    {
        let mut index = start + 2;
        let mut digits = String::new();
        while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
            if chars[index] != '_' {
                digits.push(chars[index]);
            }
            index += 1;
        }

        let parsed =
            u64::from_str_radix(&digits, radix).map_err(|_| CalcError::UnexpectedChar {
                index: start + 2,
                ch: digits.chars().next().unwrap_or('0'),
            })?;
        return Ok((parsed as f64, index));
    }

    let mut index = start;
    let mut text = String::new();
    let mut seen_dot = false;

    while index < chars.len() {
        let ch = chars[index];
        if ch == '_' {
            index += 1;
            continue;
        }
        if ch.is_ascii_digit() {
            text.push(ch);
        } else if ch == '.' && !seen_dot {
            seen_dot = true;
            text.push(ch);
        } else if (ch == 'e' || ch == 'E')
            && index + 1 < chars.len()
            && (chars[index + 1].is_ascii_digit()
                || chars[index + 1] == '+'
                || chars[index + 1] == '-')
        {
            text.push('e');
            index += 1;
            text.push(chars[index]);
        } else {
            break;
        }
        index += 1;
    }

    let value = text.parse::<f64>().map_err(|_| CalcError::UnexpectedChar {
        index: start,
        ch: chars[start],
    })?;
    Ok((value, index))
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn eat(&mut self, expected: &Token) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Result<f64, CalcError> {
        let mut value = self.term()?;

        loop {
            if self.eat(&Token::Plus) {
                value += self.term()?;
            } else if self.eat(&Token::Minus) {
                value -= self.term()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn term(&mut self) -> Result<f64, CalcError> {
        let mut value = self.unary()?;

        loop {
            if self.eat(&Token::Star) {
                value *= self.unary()?;
            } else if self.eat(&Token::Slash) {
                value /= self.unary()?;
            } else if self.eat(&Token::Percent) {
                value %= self.unary()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn unary(&mut self) -> Result<f64, CalcError> {
        if self.eat(&Token::Minus) {
            return Ok(-self.unary()?);
        }
        if self.eat(&Token::Plus) {
            return self.unary();
        }
        self.power()
    }

    fn power(&mut self) -> Result<f64, CalcError> {
        let base = self.atom()?;
        if self.eat(&Token::Caret) {
            // Right associative, and the exponent may itself be negated.
            let exponent = self.unary()?;
            return Ok(base.powf(exponent));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<f64, CalcError> {
        match self.advance().ok_or(CalcError::UnexpectedEnd)? {
            Token::Number(value) => Ok(value),
            Token::LParen => {
                let value = self.expression()?;
                if !self.eat(&Token::RParen) {
                    return Err(CalcError::UnexpectedEnd);
                }
                Ok(value)
            }
            Token::Ident(name) => self.ident(name),
            Token::Minus => Ok(-self.unary()?),
            Token::Plus => self.unary(),
            _ => Err(CalcError::Trailing {
                index: self.position - 1,
            }),
        }
    }

    fn ident(&mut self, name: String) -> Result<f64, CalcError> {
        if !self.eat(&Token::LParen) {
            return constant(&name);
        }

        let mut args = Vec::new();
        if !self.eat(&Token::RParen) {
            loop {
                args.push(self.expression()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                if self.eat(&Token::RParen) {
                    break;
                }
                return Err(CalcError::UnexpectedEnd);
            }
        }

        call(&name, &args)
    }
}

fn constant(name: &str) -> Result<f64, CalcError> {
    match name.to_lowercase().as_str() {
        "pi" => Ok(consts::PI),
        "e" => Ok(consts::E),
        "tau" => Ok(consts::TAU),
        _ => Err(CalcError::UnknownIdent(name.to_string())),
    }
}

fn call(name: &str, args: &[f64]) -> Result<f64, CalcError> {
    let lowered = name.to_lowercase();

    let unary: Option<fn(f64) -> f64> = match lowered.as_str() {
        "sqrt" => Some(f64::sqrt),
        "cbrt" => Some(f64::cbrt),
        "abs" => Some(f64::abs),
        "floor" => Some(f64::floor),
        "ceil" => Some(f64::ceil),
        "round" => Some(f64::round),
        "trunc" => Some(f64::trunc),
        "ln" => Some(f64::ln),
        "log2" => Some(f64::log2),
        "log10" => Some(f64::log10),
        "exp" => Some(f64::exp),
        "sin" => Some(f64::sin),
        "cos" => Some(f64::cos),
        "tan" => Some(f64::tan),
        "asin" => Some(f64::asin),
        "acos" => Some(f64::acos),
        "atan" => Some(f64::atan),
        "sign" => Some(f64::signum),
        _ => None,
    };
    if let Some(function) = unary {
        return match args {
            [value] => Ok(function(*value)),
            _ => Err(arity(&lowered, 1, args.len())),
        };
    }

    let binary: Option<fn(f64, f64) -> f64> = match lowered.as_str() {
        "log" => Some(f64::log),
        "atan2" => Some(f64::atan2),
        "min" => Some(f64::min),
        "max" => Some(f64::max),
        "pow" => Some(f64::powf),
        "hypot" => Some(f64::hypot),
        _ => None,
    };
    if let Some(function) = binary {
        return match args {
            [left, right] => Ok(function(*left, *right)),
            _ => Err(arity(&lowered, 2, args.len())),
        };
    }

    Err(CalcError::UnknownIdent(name.to_string()))
}

fn arity(name: &str, expected: usize, got: usize) -> CalcError {
    CalcError::BadArity {
        name: name.to_string(),
        expected,
        got,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(input: &str) -> f64 {
        eval(input).unwrap_or_else(|error| panic!("failed to evaluate {input}: {error}"))
    }

    #[test]
    fn respects_operator_precedence() {
        assert_eq!(value("2+3*4"), 14.0);
        assert_eq!(value("(2+3)*4"), 20.0);
        assert_eq!(value("10-2-3"), 5.0);
        assert_eq!(value("100/5/2"), 10.0);
        assert_eq!(value("7%3"), 1.0);
    }

    #[test]
    fn exponentiation_is_right_associative() {
        assert_eq!(value("2^3^2"), 512.0);
        assert_eq!(value("2^10"), 1024.0);
    }

    #[test]
    fn unary_minus_binds_looser_than_exponentiation() {
        assert_eq!(value("-2^2"), -4.0);
        assert_eq!(value("(-2)^2"), 4.0);
        assert_eq!(value("--3"), 3.0);
        assert_eq!(value("2^-1"), 0.5);
    }

    #[test]
    fn supports_constants_and_functions() {
        assert_eq!(value("sqrt(16)"), 4.0);
        assert_eq!(value("max(3, 9)"), 9.0);
        assert_eq!(value("log(8, 2)"), 3.0);
        assert!((value("pi") - consts::PI).abs() < f64::EPSILON);
        assert!((value("sin(0)")).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_separators_exponents_and_radix_literals() {
        assert_eq!(value("1_000_000"), 1_000_000.0);
        assert_eq!(value("1.5e3"), 1500.0);
        assert_eq!(value("2e-2"), 0.02);
        assert_eq!(value("0xff"), 255.0);
        assert_eq!(value("0b1011"), 11.0);
        assert_eq!(value("0o17"), 15.0);
    }

    #[test]
    fn division_by_zero_is_infinity_not_an_error() {
        assert_eq!(format_result(value("1/0")), "∞");
        assert_eq!(format_result(value("-1/0")), "-∞");
        assert_eq!(format_result(value("0/0")), "undefined");
    }

    #[test]
    fn formats_results_for_display() {
        assert_eq!(format_result(2.0), "2");
        assert_eq!(format_result(0.1 + 0.2), "0.3");
        assert_eq!(format_result(-0.5), "-0.5");
        assert_eq!(format_result(1.0 / 3.0), "0.3333333333");
    }

    #[test]
    fn reports_incomplete_and_trailing_input() {
        assert_eq!(eval("1+"), Err(CalcError::UnexpectedEnd));
        assert_eq!(eval(""), Err(CalcError::UnexpectedEnd));
        assert_eq!(eval("(1"), Err(CalcError::UnexpectedEnd));
        assert!(matches!(eval("1 2"), Err(CalcError::Trailing { .. })));
    }

    #[test]
    fn reports_unknown_names_and_bad_arity() {
        assert_eq!(
            eval("nope"),
            Err(CalcError::UnknownIdent("nope".to_string()))
        );
        assert_eq!(
            eval("sqrt(1, 2)"),
            Err(CalcError::BadArity {
                name: "sqrt".to_string(),
                expected: 1,
                got: 2
            })
        );
    }

    #[test]
    fn reports_unexpected_characters() {
        assert_eq!(
            eval("1 & 2"),
            Err(CalcError::UnexpectedChar { index: 2, ch: '&' })
        );
    }
}
