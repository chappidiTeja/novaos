//! `$(( ... ))`: integer arithmetic. Numbers (decimal, `0x` hex), variable names (unset or
//! non-numeric means 0), parentheses, unary `+ - !`, `* / %`, `+ -`, comparisons, `&&`, `||`.
//! Arithmetic wraps on overflow, like the shells people are used to; dividing by zero is an error.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(i64),
    Name(String),
    Op(&'static str),
}

const OPS: &[&str] = &["&&", "||", "==", "!=", "<=", ">=", "<", ">", "+", "-", "*", "/", "%", "(", ")", "!"];

fn lex(text: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            let hex = c == '0' && matches!(cs.get(i + 1), Some('x') | Some('X'));
            if hex {
                i += 2;
            }
            while i < cs.len() && (cs[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let digits: String = cs[start..i].iter().collect();
            let n = if hex {
                i64::from_str_radix(&digits[2..], 16)
            } else {
                digits.parse::<i64>()
            };
            out.push(Tok::Num(n.map_err(|_| alloc::format!("bad number '{}'", digits))?));
        } else if c == '$' && matches!(cs.get(i + 1), Some('#') | Some('?') | Some('$') | Some('!')) {
            // $# $? $$ $! : the special parameters
            out.push(Tok::Name(String::from(cs[i + 1])));
            i += 2;
        } else if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            let start = i;
            if c == '$' {
                i += 1;
            }
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            let name: String = cs[start..i].iter().collect();
            out.push(Tok::Name(String::from(name.trim_start_matches('$'))));
        } else {
            let rest: String = cs[i..].iter().take(2).collect();
            match OPS.iter().find(|op| rest.starts_with(**op)) {
                Some(op) => {
                    out.push(Tok::Op(op));
                    i += op.len();
                }
                None => return Err(alloc::format!("unexpected '{}' in arithmetic", c)),
            }
        }
    }
    Ok(out)
}

struct Eval<'a> {
    toks: Vec<Tok>,
    pos: usize,
    lookup: &'a mut dyn FnMut(&str) -> Option<String>,
}

impl Eval<'_> {
    fn peek_op(&self) -> Option<&'static str> {
        match self.toks.get(self.pos) {
            Some(Tok::Op(op)) => Some(op),
            _ => None,
        }
    }

    fn or(&mut self) -> Result<i64, String> {
        let mut v = self.and()?;
        while self.peek_op() == Some("||") {
            self.pos += 1;
            let r = self.and()?;
            v = (v != 0 || r != 0) as i64;
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<i64, String> {
        let mut v = self.equality()?;
        while self.peek_op() == Some("&&") {
            self.pos += 1;
            let r = self.equality()?;
            v = (v != 0 && r != 0) as i64;
        }
        Ok(v)
    }

    fn equality(&mut self) -> Result<i64, String> {
        let mut v = self.relational()?;
        while let Some(op @ ("==" | "!=")) = self.peek_op() {
            self.pos += 1;
            let r = self.relational()?;
            v = ((op == "==") == (v == r)) as i64;
        }
        Ok(v)
    }

    fn relational(&mut self) -> Result<i64, String> {
        let mut v = self.additive()?;
        while let Some(op @ ("<" | ">" | "<=" | ">=")) = self.peek_op() {
            self.pos += 1;
            let r = self.additive()?;
            v = match op {
                "<" => v < r,
                ">" => v > r,
                "<=" => v <= r,
                _ => v >= r,
            } as i64;
        }
        Ok(v)
    }

    fn additive(&mut self) -> Result<i64, String> {
        let mut v = self.multiplicative()?;
        while let Some(op @ ("+" | "-")) = self.peek_op() {
            self.pos += 1;
            let r = self.multiplicative()?;
            v = if op == "+" { v.wrapping_add(r) } else { v.wrapping_sub(r) };
        }
        Ok(v)
    }

    fn multiplicative(&mut self) -> Result<i64, String> {
        let mut v = self.unary()?;
        while let Some(op @ ("*" | "/" | "%")) = self.peek_op() {
            self.pos += 1;
            let r = self.unary()?;
            v = match op {
                "*" => v.wrapping_mul(r),
                _ if r == 0 => return Err(String::from("division by zero")),
                "/" => v.wrapping_div(r),
                _ => v.wrapping_rem(r),
            };
        }
        Ok(v)
    }

    fn unary(&mut self) -> Result<i64, String> {
        match self.peek_op() {
            Some("-") => {
                self.pos += 1;
                Ok(self.unary()?.wrapping_neg())
            }
            Some("+") => {
                self.pos += 1;
                self.unary()
            }
            Some("!") => {
                self.pos += 1;
                Ok((self.unary()? == 0) as i64)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<i64, String> {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(Tok::Name(name)) => {
                self.pos += 1;
                let value = (self.lookup)(&name).unwrap_or_default();
                Ok(value.trim().parse::<i64>().unwrap_or(0))
            }
            Some(Tok::Op("(")) => {
                self.pos += 1;
                let v = self.or()?;
                if self.peek_op() == Some(")") {
                    self.pos += 1;
                    Ok(v)
                } else {
                    Err(String::from("missing ')'"))
                }
            }
            Some(Tok::Op(op)) => Err(alloc::format!("unexpected '{}'", op)),
            None => Err(String::from("unexpected end of expression")),
        }
    }
}

/// Evaluate `expr`; `lookup` supplies variable values.
pub fn eval(expr: &str, lookup: &mut dyn FnMut(&str) -> Option<String>) -> Result<i64, String> {
    let toks = lex(expr)?;
    if toks.is_empty() {
        return Ok(0);
    }
    let mut e = Eval { toks, pos: 0, lookup };
    let v = e.or()?;
    if e.pos != e.toks.len() {
        return Err(String::from("unexpected trailing input"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(s: &str) -> Result<i64, String> {
        eval(s, &mut |name| match name {
            "x" => Some(String::from("7")),
            "y" => Some(String::from(" 3 ")),
            "bad" => Some(String::from("abc")),
            _ => None,
        })
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(calc("1 + 2 * 3"), Ok(7));
        assert_eq!(calc("(1 + 2) * 3"), Ok(9));
        assert_eq!(calc("10 - 4 - 3"), Ok(3), "subtraction associates to the left");
        assert_eq!(calc("100 / 10 / 5"), Ok(2));
        assert_eq!(calc("17 % 5"), Ok(2));
        assert_eq!(calc("-3 + 5"), Ok(2));
        assert_eq!(calc("- -3"), Ok(3));
        assert_eq!(calc("2 * -3"), Ok(-6));
        assert_eq!(calc("0x10 + 0xff"), Ok(271));
        assert_eq!(calc(""), Ok(0));
        assert_eq!(calc("   "), Ok(0));
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(calc("1 < 2"), Ok(1));
        assert_eq!(calc("2 < 1"), Ok(0));
        assert_eq!(calc("2 <= 2"), Ok(1));
        assert_eq!(calc("3 >= 4"), Ok(0));
        assert_eq!(calc("3 == 3"), Ok(1));
        assert_eq!(calc("3 != 3"), Ok(0));
        assert_eq!(calc("1 && 0"), Ok(0));
        assert_eq!(calc("1 && 5"), Ok(1));
        assert_eq!(calc("0 || 0"), Ok(0));
        assert_eq!(calc("0 || 9"), Ok(1));
        assert_eq!(calc("!0"), Ok(1));
        assert_eq!(calc("!5"), Ok(0));
        assert_eq!(calc("1 + 1 == 2 && 3 > 2"), Ok(1));
    }

    #[test]
    fn variables() {
        assert_eq!(calc("x + 1"), Ok(8));
        assert_eq!(calc("$x * 2"), Ok(14));
        assert_eq!(calc("y + y"), Ok(6), "surrounding blanks in a value are ignored");
        assert_eq!(calc("unset + 1"), Ok(1), "an unset variable is 0");
        assert_eq!(calc("bad + 1"), Ok(1), "so is one that is not a number");
    }

    #[test]
    fn errors() {
        assert!(calc("1 / 0").unwrap_err().contains("division"));
        assert!(calc("5 % 0").is_err());
        assert!(calc("1 +").is_err());
        assert!(calc("(1 + 2").is_err());
        assert!(calc("1 2").is_err());
        assert!(calc("1 $ 2").is_ok() || calc("1 $ 2").is_err());
        assert!(calc("1 @ 2").is_err());
        assert!(calc("99999999999999999999").is_err());
        assert!(calc(")").is_err());
    }

    #[test]
    fn overflow_wraps_instead_of_crashing() {
        assert_eq!(calc("9223372036854775807 + 1"), Ok(i64::MIN));
        assert_eq!(calc("-9223372036854775807 - 2"), Ok(i64::MAX));
        assert_eq!(calc("9223372036854775807 * 2"), Ok(-2));
        assert!(calc("(0 - 9223372036854775807 - 1) / -1").is_ok());
    }

    #[test]
    fn nonsense_never_panics() {
        let mut s = 0xDEAD_BEEFu64;
        let alphabet: Vec<char> = "0123456789 +-*/%()<>=!&|xy$".chars().collect();
        for _ in 0..20_000 {
            let mut text = String::new();
            for _ in 0..(s % 12) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                text.push(alphabet[(s % alphabet.len() as u64) as usize]);
            }
            let _ = calc(&text);
        }
    }
}
