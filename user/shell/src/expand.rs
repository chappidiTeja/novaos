//! Turning a parsed word into the strings a command receives: variables, command
//! substitution, arithmetic, `~`, splitting unquoted results at blanks, and wildcards.

use crate::arith;
use crate::glob;
use crate::syntax::{Part, VarOp, Word};
use alloc::string::String;
use alloc::vec::Vec;

/// What expansion needs from the running shell.
pub trait Context {
    /// An ordinary variable; None if it is not set.
    fn var(&mut self, name: &str) -> Option<String>;
    fn set_var(&mut self, name: &str, value: &str);
    /// `?`, `$`, `#`, `!`, `0`..`9`, `@`, `*`. `@` and `*` give every positional parameter.
    fn special(&mut self, name: &str) -> Option<Vec<String>>;
    /// Run a command and return what it wrote to standard output.
    fn command_output(&mut self, command: &str) -> String;
    /// Names in a directory ("" means the current one); empty if it cannot be read.
    fn list_dir(&mut self, dir: &str) -> Vec<String>;
    fn is_dir(&mut self, path: &str) -> bool;
    fn exists(&mut self, path: &str) -> bool;
    fn home(&mut self) -> String;
    /// An arithmetic or other expansion went wrong; the message is for the user.
    fn error(&mut self, message: &str);
}

fn is_special(name: &str) -> bool {
    matches!(name, "?" | "$" | "#" | "@" | "*" | "!" | "-") || name.chars().all(|c| c.is_ascii_digit())
}

/// A field being built: its text, and the same with wildcards that must stay literal escaped.
#[derive(Default, Clone)]
struct Field {
    text: String,
    pattern: String,
    /// There is an unquoted wildcard in `pattern`.
    glob: bool,
    /// Something quoted contributed (even an empty string), so the field exists even if empty.
    solid: bool,
}

impl Field {
    fn add_quoted(&mut self, s: &str) {
        self.text.push_str(s);
        self.pattern.push_str(&glob::escape(s));
        self.solid = true;
    }

    fn add_unquoted(&mut self, s: &str) {
        self.text.push_str(s);
        self.pattern.push_str(s);
        if glob::has_wildcards(s) {
            self.glob = true;
        }
    }

    /// Text that came out of an expansion and is not subject to wildcards (it was ~ or similar).
    fn add_literal(&mut self, s: &str) {
        self.text.push_str(s);
        self.pattern.push_str(&glob::escape(s));
    }
}

fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

struct Builder {
    fields: Vec<Field>,
    current: Option<Field>,
}

impl Builder {
    fn new() -> Builder {
        Builder { fields: Vec::new(), current: None }
    }

    fn cur(&mut self) -> &mut Field {
        self.current.get_or_insert_with(Field::default)
    }

    /// End the field being built (empty unquoted fields vanish).
    fn end_field(&mut self) {
        if let Some(f) = self.current.take() {
            if !f.text.is_empty() || f.solid {
                self.fields.push(f);
            }
        }
    }

    /// Add the value of an unquoted expansion: it is split at blanks.
    fn add_split(&mut self, value: &str) {
        let mut chars = value.chars().peekable();
        let mut word = String::new();
        let mut started = false;
        while let Some(c) = chars.next() {
            if is_blank(c) {
                if !word.is_empty() {
                    self.cur().add_unquoted(&word);
                    word.clear();
                }
                if started || self.current.is_some() {
                    self.end_field();
                }
                started = false;
                // swallow the run of blanks
                while chars.peek().map_or(false, |&n| is_blank(n)) {
                    chars.next();
                }
            } else {
                word.push(c);
                started = true;
            }
        }
        if !word.is_empty() {
            self.cur().add_unquoted(&word);
        }
    }
}

/// The value of a variable reference, or the list for `@`/`*`.
fn lookup(ctx: &mut dyn Context, name: &str) -> Option<Vec<String>> {
    if is_special(name) {
        ctx.special(name)
    } else {
        ctx.var(name).map(|v| alloc::vec![v])
    }
}

fn join(values: &[String]) -> String {
    values.join(" ")
}

/// The text of one `Var` part with its operator applied. For `$@` unquoted or quoted the caller
/// handles the list; this returns the joined text.
fn var_value(ctx: &mut dyn Context, name: &str, op: &VarOp) -> String {
    let current = lookup(ctx, name);
    let text = current.as_ref().map(|v| join(v));
    let empty = text.as_ref().map_or(true, |t| t.is_empty());
    match op {
        VarOp::Plain => text.unwrap_or_default(),
        VarOp::Length => {
            let n = match (name, &current) {
                ("@" | "*", Some(list)) => list.len(),
                (_, Some(list)) => join(list).chars().count(),
                _ => 0,
            };
            alloc::format!("{}", n)
        }
        VarOp::Default(word, colon) => {
            if text.is_none() || (*colon && empty) {
                expand_single(ctx, word)
            } else {
                text.unwrap_or_default()
            }
        }
        VarOp::Alternative(word, colon) => {
            if text.is_some() && !(*colon && empty) {
                expand_single(ctx, word)
            } else {
                String::new()
            }
        }
        VarOp::Assign(word, colon) => {
            if text.is_none() || (*colon && empty) {
                let value = expand_single(ctx, word);
                ctx.set_var(name, &value);
                value
            } else {
                text.unwrap_or_default()
            }
        }
    }
}

/// Command output with trailing newlines removed.
fn substitute(ctx: &mut dyn Context, command: &str) -> String {
    let mut out = ctx.command_output(command);
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn arithmetic(ctx: &mut dyn Context, expr: &str) -> String {
    // Variables are looked up through the context while the expression is evaluated; $1, $#...
    // are positional or special parameters, not ordinary variables.
    let mut get = |name: &str| {
        if is_special(name) {
            ctx.special(name).map(|v| join(&v))
        } else {
            ctx.var(name)
        }
    };
    match arith::eval(expr, &mut get) {
        Ok(v) => alloc::format!("{}", v),
        Err(e) => {
            ctx.error(&alloc::format!("arithmetic: {}", e));
            String::from("0")
        }
    }
}

/// Expand a word into the fields a command line gets (blanks split unquoted results, wildcards
/// are replaced by matching file names).
pub fn expand_word(ctx: &mut dyn Context, word: &Word) -> Vec<String> {
    let mut b = Builder::new();
    for wp in &word.0 {
        match &wp.part {
            Part::Lit(s) => {
                if wp.quoted {
                    b.cur().add_quoted(s);
                } else {
                    b.cur().add_unquoted(s);
                }
            }
            Part::Tilde => {
                let home = ctx.home();
                b.cur().add_literal(&home);
            }
            Part::Var { name, op } => {
                if matches!(op, VarOp::Plain) && (name == "@" || name == "*") {
                    let list = ctx.special(name).unwrap_or_default();
                    if wp.quoted && name == "@" {
                        // "$@": every parameter is a field of its own.
                        for (i, item) in list.iter().enumerate() {
                            if i > 0 {
                                b.end_field();
                            }
                            b.cur().add_quoted(item);
                        }
                    } else if wp.quoted {
                        b.cur().add_quoted(&join(&list));
                    } else {
                        b.add_split(&join(&list));
                    }
                } else {
                    let value = var_value(ctx, name, op);
                    if wp.quoted {
                        b.cur().add_quoted(&value);
                    } else {
                        b.add_split(&value);
                    }
                }
            }
            Part::CmdSub(cmd) => {
                let out = substitute(ctx, cmd);
                if wp.quoted {
                    b.cur().add_quoted(&out);
                } else {
                    b.add_split(&out);
                }
            }
            Part::Arith(expr) => {
                let v = arithmetic(ctx, expr);
                if wp.quoted {
                    b.cur().add_quoted(&v);
                } else {
                    b.add_split(&v);
                }
            }
        }
    }
    b.end_field();

    let mut out = Vec::new();
    for f in b.fields {
        if f.glob {
            let found = expand_pattern(ctx, &f.pattern);
            if found.is_empty() {
                out.push(f.text);
            } else {
                out.extend(found);
            }
        } else {
            out.push(f.text);
        }
    }
    out
}

/// Expand a word to one string, with no splitting and no wildcards (assignments, redirection
/// targets, the words inside `${x:-word}`).
pub fn expand_single(ctx: &mut dyn Context, word: &Word) -> String {
    let mut out = String::new();
    for wp in &word.0 {
        match &wp.part {
            Part::Lit(s) => out.push_str(s),
            Part::Tilde => out.push_str(&ctx.home()),
            Part::Var { name, op } => out.push_str(&var_value(ctx, name, op)),
            Part::CmdSub(cmd) => out.push_str(&substitute(ctx, cmd)),
            Part::Arith(expr) => out.push_str(&arithmetic(ctx, expr)),
        }
    }
    out
}

/// File names matching a pattern, sorted; empty if none do.
fn expand_pattern(ctx: &mut dyn Context, pattern: &str) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    let mut paths: Vec<String> = alloc::vec![String::from(if absolute { "/" } else { "" })];
    for (i, comp) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        let mut next = Vec::new();
        if glob::has_wildcards(comp) {
            for dir in &paths {
                let mut names = ctx.list_dir(dir);
                names.sort();
                for name in names {
                    if name.starts_with('.') && !comp.starts_with('.') {
                        continue;
                    }
                    if !glob::matches(comp, &name) {
                        continue;
                    }
                    let full = alloc::format!("{}{}", dir, name);
                    if last || ctx.is_dir(&full) {
                        next.push(if last { full } else { alloc::format!("{}/", full) });
                    }
                }
            }
        } else {
            let literal = glob::unescape(comp);
            for dir in &paths {
                let full = alloc::format!("{}{}", dir, literal);
                next.push(if last { full } else { alloc::format!("{}/", full) });
            }
        }
        paths = next;
        if paths.is_empty() {
            return paths;
        }
    }
    // A name that ends in a literal component (after a wildcard directory part) was not read from
    // a directory listing, so it has to be checked.
    if comps.last().map_or(false, |c| !glob::has_wildcards(c)) {
        paths.retain(|p| ctx.exists(p));
    }
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{parse, Command};
    use alloc::collections::BTreeMap;

    struct Fake {
        vars: BTreeMap<String, String>,
        params: Vec<String>,
        fs: Vec<&'static str>, // directories end in '/'
        errors: Vec<String>,
    }

    impl Fake {
        /// A path as the fake file system sees it: relative names are under "/", the working directory.
        fn absolute(path: &str) -> String {
            if path.starts_with('/') {
                String::from(path)
            } else {
                alloc::format!("/{}", path)
            }
        }

        fn new() -> Fake {
            let mut vars = BTreeMap::new();
            vars.insert("name".into(), "nova".into());
            vars.insert("empty".into(), "".into());
            vars.insert("spaced".into(), "  a   b  ".into());
            vars.insert("stars".into(), "*.txt".into());
            vars.insert("n".into(), "6".into());
            Fake {
                vars,
                params: alloc::vec!["one".into(), "two words".into(), "three".into()],
                fs: alloc::vec!["/a.txt", "/b.txt", "/c.rs", "/.hidden", "/dir/", "/dir/x.txt", "/dir/y.rs", "/dir/sub/", "/dir/sub/z.txt"],
                errors: Vec::new(),
            }
        }
    }

    impl Context for Fake {
        fn var(&mut self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn set_var(&mut self, name: &str, value: &str) {
            self.vars.insert(name.into(), value.into());
        }
        fn special(&mut self, name: &str) -> Option<Vec<String>> {
            match name {
                "?" => Some(alloc::vec!["0".into()]),
                "$" => Some(alloc::vec!["42".into()]),
                "#" => Some(alloc::vec![alloc::format!("{}", self.params.len())]),
                "@" | "*" => Some(self.params.clone()),
                "0" => Some(alloc::vec!["sh".into()]),
                d => d.parse::<usize>().ok().and_then(|i| self.params.get(i - 1)).map(|p| alloc::vec![p.clone()]),
            }
        }
        fn command_output(&mut self, command: &str) -> String {
            match command {
                "echo hi" => "hi\n\n".into(),
                "words" => "x y\nz".into(),
                _ => String::new(),
            }
        }
        fn list_dir(&mut self, dir: &str) -> Vec<String> {
            let dir = if dir.is_empty() { "/" } else { dir };
            let mut out = Vec::new();
            for entry in &self.fs {
                if let Some(rest) = entry.strip_prefix(dir) {
                    let rest = rest.trim_end_matches('/');
                    if !rest.is_empty() && !rest.contains('/') {
                        out.push(String::from(rest));
                    }
                }
            }
            out
        }
        fn is_dir(&mut self, path: &str) -> bool {
            let p = alloc::format!("{}/", Self::absolute(path).trim_end_matches('/'));
            self.fs.iter().any(|e| *e == p)
        }
        fn exists(&mut self, path: &str) -> bool {
            let p = Self::absolute(path);
            let p = p.trim_end_matches('/');
            self.fs.iter().any(|e| e.trim_end_matches('/') == p)
        }
        fn home(&mut self) -> String {
            "/home".into()
        }
        fn error(&mut self, message: &str) {
            self.errors.push(message.into());
        }
    }

    /// The fields the words of a simple command expand to.
    fn run(text: &str) -> Vec<String> {
        let mut ctx = Fake::new();
        run_with(&mut ctx, text)
    }

    fn run_with(ctx: &mut Fake, text: &str) -> Vec<String> {
        let list = parse(text).unwrap();
        let Command::Simple { words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        words.iter().flat_map(|w| expand_word(ctx, w)).collect()
    }

    #[test]
    fn plain_and_quoted_words() {
        assert_eq!(run("e a b"), ["e", "a", "b"]);
        assert_eq!(run("e 'a  b' c"), ["e", "a  b", "c"]);
        assert_eq!(run("e \"a  b\" c"), ["e", "a  b", "c"]);
        assert_eq!(run("e ''"), ["e", ""], "an empty quoted word survives");
        assert_eq!(run("e a\"\"b"), ["e", "ab"]);
    }

    #[test]
    fn variables_and_splitting() {
        assert_eq!(run("e $name"), ["e", "nova"]);
        assert_eq!(run("e ${name}x"), ["e", "novax"]);
        assert_eq!(run("e \"$name\""), ["e", "nova"]);
        assert_eq!(run("e $unset x"), ["e", "x"], "an unset variable vanishes");
        assert_eq!(run("e \"$unset\" x"), ["e", "", "x"], "but quoted it is an empty word");
        assert_eq!(run("e $empty x"), ["e", "x"]);
        assert_eq!(run("e $spaced"), ["e", "a", "b"], "unquoted values split at blanks");
        assert_eq!(run("e \"$spaced\""), ["e", "  a   b  "]);
        assert_eq!(run("e x$spaced"), ["e", "x", "a", "b"], "a leading blank in the value ends the text before it");
        assert_eq!(run("e $spaced\"y\""), ["e", "a", "b", "y"], "and a trailing blank ends the field");
        assert_eq!(run("e '$name'"), ["e", "$name"]);
        assert_eq!(run("e \\$name"), ["e", "$name"]);
    }

    #[test]
    fn positional_parameters() {
        assert_eq!(run("e $1 $3"), ["e", "one", "three"]);
        assert_eq!(run("e $#"), ["e", "3"]);
        assert_eq!(run("e $@"), ["e", "one", "two", "words", "three"]);
        assert_eq!(run("e \"$@\""), ["e", "one", "two words", "three"]);
        assert_eq!(run("e \"$*\""), ["e", "one two words three"]);
        assert_eq!(run("e $0 $? $$"), ["e", "sh", "0", "42"]);
        assert_eq!(run("e \"x$@\""), ["e", "xone", "two words", "three"]);
        let mut ctx = Fake::new();
        ctx.params.clear();
        assert_eq!(run_with(&mut ctx, "e \"$@\""), ["e"], "\"$@\" with no parameters is nothing");
        assert_eq!(run_with(&mut ctx, "e \"$@\"x"), ["e", "x"]);
    }

    #[test]
    fn variable_operators() {
        assert_eq!(run("e ${unset:-fallback}"), ["e", "fallback"]);
        assert_eq!(run("e ${empty:-fallback}"), ["e", "fallback"]);
        assert_eq!(run("e ${empty-fallback}"), ["e"], "without the colon an empty value counts as set");
        assert_eq!(run("e ${name:-fallback}"), ["e", "nova"]);
        assert_eq!(run("e ${name:+alt}"), ["e", "alt"]);
        assert_eq!(run("e ${unset:+alt} x"), ["e", "x"]);
        assert_eq!(run("e ${#name}"), ["e", "4"]);
        assert_eq!(run("e ${#@}"), ["e", "3"]);
        assert_eq!(run("e ${unset:-$name}"), ["e", "nova"]);
        let mut ctx = Fake::new();
        assert_eq!(run_with(&mut ctx, "e ${later:=made} $later"), ["e", "made", "made"]);
        assert_eq!(ctx.vars["later"], "made");
    }

    #[test]
    fn command_substitution() {
        assert_eq!(run("e $(echo hi)"), ["e", "hi"], "trailing newlines go");
        assert_eq!(run("e \"$(echo hi)\""), ["e", "hi"]);
        assert_eq!(run("e $(words)"), ["e", "x", "y", "z"], "unquoted output splits at any blank");
        assert_eq!(run("e \"$(words)\""), ["e", "x y\nz"]);
        assert_eq!(run("e `echo hi`"), ["e", "hi"]);
        assert_eq!(run("e $(nothing) x"), ["e", "x"]);
    }

    #[test]
    fn arithmetic_expansion() {
        assert_eq!(run("e $((1+2))"), ["e", "3"]);
        assert_eq!(run("e $((n * 7))"), ["e", "42"]);
        assert_eq!(run("e $(( n / 4 ))x"), ["e", "1x"]);
        assert_eq!(run("e $(($1 + $#))"), ["e", "3"], "positional parameters count as numbers");
        let mut ctx = Fake::new();
        assert_eq!(run_with(&mut ctx, "e $((1/0))"), ["e", "0"]);
        assert_eq!(ctx.errors.len(), 1);
        assert!(ctx.errors[0].contains("division"));
    }

    #[test]
    fn tilde() {
        assert_eq!(run("e ~ ~/x"), ["e", "/home", "/home/x"]);
        assert_eq!(run("e '~'"), ["e", "~"]);
    }

    #[test]
    fn wildcards() {
        assert_eq!(run("e *.txt"), ["e", "a.txt", "b.txt"]);
        assert_eq!(run("e /*.rs"), ["e", "/c.rs"]);
        assert_eq!(run("e /dir/*"), ["e", "/dir/sub", "/dir/x.txt", "/dir/y.rs"]);
        assert_eq!(run("e /*/*.txt"), ["e", "/dir/x.txt"]);
        assert_eq!(run("e /dir/*/z.txt"), ["e", "/dir/sub/z.txt"]);
        assert_eq!(run("e ?.rs"), ["e", "c.rs"]);
        assert_eq!(run("e [ab].txt"), ["e", "a.txt", "b.txt"]);
        assert_eq!(run("e *"), ["e", "a.txt", "b.txt", "c.rs", "dir"], "hidden files need an explicit dot");
        assert_eq!(run("e .h*"), ["e", ".hidden"]);
        // No match: the pattern itself.
        assert_eq!(run("e *.zzz"), ["e", "*.zzz"]);
        assert_eq!(run("e /nodir/*"), ["e", "/nodir/*"]);
        // Quoting switches wildcards off.
        assert_eq!(run("e '*.txt'"), ["e", "*.txt"]);
        assert_eq!(run("e \"*.txt\""), ["e", "*.txt"]);
        assert_eq!(run("e \\*.txt"), ["e", "*.txt"]);
        // A wildcard that came from a variable is active unless the variable is quoted.
        assert_eq!(run("e $stars"), ["e", "a.txt", "b.txt"]);
        assert_eq!(run("e \"$stars\""), ["e", "*.txt"]);
    }

    #[test]
    fn single_expansion_does_not_split_or_glob() {
        let mut ctx = Fake::new();
        let list = parse("A=$spaced$stars").unwrap();
        let Command::Simple { assigns, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(expand_single(&mut ctx, &assigns[0].1), "  a   b  *.txt");
    }
}
