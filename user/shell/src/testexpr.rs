//! The `test` / `[` command: file tests, string tests and integer comparisons, combined with
//! `!`, `-a`, `-o` and parentheses.

use alloc::string::String;

/// What the file tests need to know about the world.
pub trait TestFs {
    fn exists(&mut self, path: &str) -> bool;
    fn is_file(&mut self, path: &str) -> bool;
    fn is_dir(&mut self, path: &str) -> bool;
    fn size(&mut self, path: &str) -> Option<u64>;
}

struct T<'a, F: TestFs> {
    args: &'a [String],
    pos: usize,
    fs: &'a mut F,
}

fn number(s: &str) -> Result<i64, String> {
    s.trim().parse::<i64>().map_err(|_| alloc::format!("{}: integer expression expected", s))
}

impl<F: TestFs> T<'_, F> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(|s| s.as_str())
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut v = self.and()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut v = self.not()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            let r = self.not()?;
            v = v && r;
        }
        Ok(v)
    }

    fn not(&mut self) -> Result<bool, String> {
        if self.peek() == Some("!") && self.args.len() - self.pos > 1 {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        let Some(first) = self.peek().map(String::from) else {
            return Err(String::from("argument expected"));
        };
        if first == "(" {
            self.pos += 1;
            let v = self.or()?;
            if self.peek() != Some(")") {
                return Err(String::from("')' expected"));
            }
            self.pos += 1;
            return Ok(v);
        }
        // Binary operator: arg OP arg
        if let Some(op) = self.args.get(self.pos + 1).map(|s| s.as_str()) {
            if let Some(rhs) = self.args.get(self.pos + 2) {
                let rhs = rhs.clone();
                let result = match op {
                    "=" | "==" => Some(first == rhs),
                    "!=" => Some(first != rhs),
                    "<" => Some(first < rhs),
                    ">" => Some(first > rhs),
                    "-eq" => Some(number(&first)? == number(&rhs)?),
                    "-ne" => Some(number(&first)? != number(&rhs)?),
                    "-lt" => Some(number(&first)? < number(&rhs)?),
                    "-le" => Some(number(&first)? <= number(&rhs)?),
                    "-gt" => Some(number(&first)? > number(&rhs)?),
                    "-ge" => Some(number(&first)? >= number(&rhs)?),
                    _ => None,
                };
                if let Some(r) = result {
                    self.pos += 3;
                    return Ok(r);
                }
            }
        }
        // Unary operator: OP arg
        if first.starts_with('-') && first.len() == 2 {
            if let Some(arg) = self.args.get(self.pos + 1).cloned() {
                let r = match first.as_str() {
                    "-n" => Some(!arg.is_empty()),
                    "-z" => Some(arg.is_empty()),
                    "-e" => Some(self.fs.exists(&arg)),
                    "-f" => Some(self.fs.is_file(&arg)),
                    "-d" => Some(self.fs.is_dir(&arg)),
                    "-s" => Some(self.fs.size(&arg).map_or(false, |s| s > 0)),
                    // Permission bits do not exist yet: anything that exists is readable, writable
                    // and (for files) runnable.
                    "-r" | "-w" | "-x" => Some(self.fs.exists(&arg)),
                    _ => None,
                };
                if let Some(r) = r {
                    self.pos += 2;
                    return Ok(r);
                }
            }
        }
        // A lone word is true if it is not empty.
        self.pos += 1;
        Ok(!first.is_empty())
    }
}

/// Evaluate the arguments of `test` (without the closing `]`).
pub fn evaluate<F: TestFs>(args: &[String], fs: &mut F) -> Result<bool, String> {
    if args.is_empty() {
        return Ok(false);
    }
    let mut t = T { args, pos: 0, fs };
    let v = t.or()?;
    if t.pos != args.len() {
        return Err(alloc::format!("unexpected argument '{}'", args[t.pos]));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl TestFs for Fake {
        fn exists(&mut self, p: &str) -> bool {
            matches!(p, "/etc" | "/etc/motd" | "/empty")
        }
        fn is_file(&mut self, p: &str) -> bool {
            matches!(p, "/etc/motd" | "/empty")
        }
        fn is_dir(&mut self, p: &str) -> bool {
            p == "/etc"
        }
        fn size(&mut self, p: &str) -> Option<u64> {
            match p {
                "/etc/motd" => Some(12),
                "/empty" => Some(0),
                _ => None,
            }
        }
    }

    fn t(args: &[&str]) -> Result<bool, String> {
        let v: Vec<String> = args.iter().map(|s| String::from(*s)).collect();
        evaluate(&v, &mut Fake)
    }

    #[test]
    fn files() {
        assert_eq!(t(&["-e", "/etc"]), Ok(true));
        assert_eq!(t(&["-e", "/nope"]), Ok(false));
        assert_eq!(t(&["-f", "/etc/motd"]), Ok(true));
        assert_eq!(t(&["-f", "/etc"]), Ok(false));
        assert_eq!(t(&["-d", "/etc"]), Ok(true));
        assert_eq!(t(&["-d", "/etc/motd"]), Ok(false));
        assert_eq!(t(&["-s", "/etc/motd"]), Ok(true));
        assert_eq!(t(&["-s", "/empty"]), Ok(false));
        assert_eq!(t(&["-s", "/nope"]), Ok(false));
        assert_eq!(t(&["-r", "/etc/motd"]), Ok(true));
    }

    #[test]
    fn strings() {
        assert_eq!(t(&["abc"]), Ok(true));
        assert_eq!(t(&[""]), Ok(false));
        assert_eq!(t(&["-n", "x"]), Ok(true));
        assert_eq!(t(&["-n", ""]), Ok(false));
        assert_eq!(t(&["-z", ""]), Ok(true));
        assert_eq!(t(&["a", "=", "a"]), Ok(true));
        assert_eq!(t(&["a", "==", "b"]), Ok(false));
        assert_eq!(t(&["a", "!=", "b"]), Ok(true));
        assert_eq!(t(&["a", "<", "b"]), Ok(true));
        assert_eq!(t(&["b", ">", "a"]), Ok(true));
        assert_eq!(t(&[]), Ok(false));
        // A word that looks like an operator is still just a word when alone.
        assert_eq!(t(&["-n"]), Ok(true));
        assert_eq!(t(&["!"]), Ok(true));
        assert_eq!(t(&["-f"]), Ok(true));
    }

    #[test]
    fn integers() {
        assert_eq!(t(&["3", "-eq", "3"]), Ok(true));
        assert_eq!(t(&["3", "-ne", "3"]), Ok(false));
        assert_eq!(t(&["2", "-lt", "10"]), Ok(true), "numerically, not as text");
        assert_eq!(t(&["10", "-le", "10"]), Ok(true));
        assert_eq!(t(&["10", "-gt", "9"]), Ok(true));
        assert_eq!(t(&["9", "-ge", "10"]), Ok(false));
        assert_eq!(t(&[" 4 ", "-eq", "4"]), Ok(true));
        assert!(t(&["a", "-eq", "1"]).is_err());
        assert!(t(&["1", "-eq", ""]).is_err());
    }

    #[test]
    fn combinations() {
        assert_eq!(t(&["!", "-e", "/nope"]), Ok(true));
        assert_eq!(t(&["!", "!", "-e", "/etc"]), Ok(true));
        assert_eq!(t(&["-e", "/etc", "-a", "-d", "/etc"]), Ok(true));
        assert_eq!(t(&["-e", "/etc", "-a", "-f", "/etc"]), Ok(false));
        assert_eq!(t(&["-f", "/etc", "-o", "-d", "/etc"]), Ok(true));
        assert_eq!(t(&["(", "-f", "/etc", "-o", "-d", "/etc", ")", "-a", "a", "=", "a"]), Ok(true));
        assert!(t(&["(", "-e", "/etc"]).is_err());
        assert!(t(&["-e", "/etc", "extra"]).is_err());
        assert!(t(&["a", "=", "a", "b"]).is_err());
    }
}
