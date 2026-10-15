//! The shell language: a lexer that turns text into words and operators, and a parser that
//! builds commands from them.
//!
//! Supported: pipelines (`|`), lists (`;`, newline, `&&`, `||`, `&`), redirections (`<`, `>`,
//! `>>`, `2>`, `2>&1`, `&>`), quoting (`'...'`, `"..."`, backslash), `$name` / `${name}` /
//! `${name:-default}` / `${#name}`, `$?` `$$` `$#` `$@` `$*` `$1`..., `$(command)` and `` `command` ``,
//! `$((arithmetic))`, `~`, and the compound commands `if`, `while`, `until`, `for`, `{ ...; }`
//! and function definitions. Not supported: `case`, here-documents, subshells `( ... )`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

// ---- the tree ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum VarOp {
    /// `$name` / `${name}`
    Plain,
    /// `${name:-word}` (or `${name-word}` when `colon` is false): `word` if unset (or empty).
    Default(Word, bool),
    /// `${name:+word}`: `word` if set (and not empty).
    Alternative(Word, bool),
    /// `${name:=word}`: assign `word` if unset (or empty), then use it.
    Assign(Word, bool),
    /// `${#name}`: length of the value.
    Length,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// Text, already free of quotes and escapes.
    Lit(String),
    Var { name: String, op: VarOp },
    /// `$(...)` or backquotes: the command's text.
    CmdSub(String),
    /// `$((...))`: the expression's text.
    Arith(String),
    /// A leading `~`.
    Tilde,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WordPart {
    pub part: Part,
    /// Inside quotes: no splitting into fields, no wildcards.
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word(pub Vec<WordPart>);

impl Word {
    pub fn literal(s: &str) -> Word {
        Word(alloc::vec![WordPart { part: Part::Lit(String::from(s)), quoted: false }])
    }

    /// The text if the word is nothing but unquoted literal text (so `if`, `then`... can be
    /// recognised and `name=value` taken apart).
    pub fn as_plain(&self) -> Option<&str> {
        match self.0.as_slice() {
            [WordPart { part: Part::Lit(s), quoted: false }] => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirOp {
    /// `< file`
    In,
    /// `> file`
    Out,
    /// `>> file`
    Append,
    /// `>& n`: this descriptor becomes a copy of descriptor n.
    DupOut,
    /// `&> file`: standard output and standard error to the file.
    OutAndErr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Redir {
    pub fd: u32,
    pub op: RedirOp,
    pub target: Word,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Simple { assigns: Vec<(String, Word)>, words: Vec<Word>, redirs: Vec<Redir> },
    If { arms: Vec<(List, List)>, otherwise: Option<List> },
    While { cond: List, body: List, until: bool },
    For { var: String, items: Option<Vec<Word>>, body: List },
    Group(List),
    Function { name: String, body: Box<Command> },
    /// A compound command followed by redirections: `while ...; done > file`.
    Redirected { command: Box<Command>, redirs: Vec<Redir> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub negate: bool,
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Connector, Pipeline)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub and_or: AndOr,
    /// Followed by `&`: run in the background.
    pub background: bool,
}

pub type List = Vec<Item>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The text ends in the middle of a construct (open quote, unfinished `if`...): more input
    /// would complete it.
    Incomplete,
    Syntax(String),
}

fn syntax<T>(msg: &str) -> Result<T, ParseError> {
    Err(ParseError::Syntax(String::from(msg)))
}

// ---- the lexer ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Pipe,
    OrIf,
    AndIf,
    Semi,
    DSemi,
    Amp,
    LParen,
    RParen,
    Less,
    Great,
    DGreat,
    GreatAnd,
    AndGreat,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(Word),
    /// A number directly in front of a redirection operator (`2>`).
    IoNumber(u32),
    Op(Op),
    Newline,
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_delimiter(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

struct Lexer {
    cs: Vec<char>,
    i: usize,
}

impl Lexer {
    fn new(text: &str) -> Lexer {
        Lexer { cs: text.chars().collect(), i: 0 }
    }

    fn peek(&self, k: usize) -> Option<char> {
        self.cs.get(self.i + k).copied()
    }

    fn tokens(&mut self) -> Result<Vec<Tok>, ParseError> {
        let mut out = Vec::new();
        loop {
            // Blanks, line continuations, comments.
            while let Some(c) = self.peek(0) {
                if c == ' ' || c == '\t' {
                    self.i += 1;
                } else if c == '\\' && self.peek(1) == Some('\n') {
                    self.i += 2;
                } else {
                    break;
                }
            }
            let Some(c) = self.peek(0) else { return Ok(out) };
            if c == '#' {
                while self.peek(0).map_or(false, |c| c != '\n') {
                    self.i += 1;
                }
                continue;
            }
            if c == '\n' {
                self.i += 1;
                out.push(Tok::Newline);
                continue;
            }
            // A number immediately followed by < or > is a descriptor number.
            if c.is_ascii_digit() {
                let mut j = 0;
                while self.peek(j).map_or(false, |d| d.is_ascii_digit()) {
                    j += 1;
                }
                if matches!(self.peek(j), Some('<') | Some('>')) && j <= 2 {
                    let n: u32 = self.cs[self.i..self.i + j].iter().collect::<String>().parse().unwrap_or(0);
                    self.i += j;
                    out.push(Tok::IoNumber(n));
                    continue;
                }
            }
            if let Some(op) = self.operator()? {
                out.push(Tok::Op(op));
                continue;
            }
            out.push(Tok::Word(self.word()?));
        }
    }

    fn operator(&mut self) -> Result<Option<Op>, ParseError> {
        let c = self.peek(0).unwrap();
        let next = self.peek(1);
        let (op, len) = match (c, next) {
            ('|', Some('|')) => (Op::OrIf, 2),
            ('|', _) => (Op::Pipe, 1),
            ('&', Some('&')) => (Op::AndIf, 2),
            ('&', Some('>')) => (Op::AndGreat, 2),
            ('&', _) => (Op::Amp, 1),
            (';', Some(';')) => (Op::DSemi, 2),
            (';', _) => (Op::Semi, 1),
            ('(', _) => (Op::LParen, 1),
            (')', _) => (Op::RParen, 1),
            ('<', Some('<')) => return syntax("here-documents are not supported"),
            ('<', _) => (Op::Less, 1),
            ('>', Some('>')) => (Op::DGreat, 2),
            ('>', Some('&')) => (Op::GreatAnd, 2),
            ('>', _) => (Op::Great, 1),
            _ => return Ok(None),
        };
        self.i += len;
        Ok(Some(op))
    }

    /// One word: everything up to a blank or operator, with quotes and expansions parsed.
    fn word(&mut self) -> Result<Word, ParseError> {
        let mut b = WordBuilder::default();
        let start = self.i;
        while let Some(c) = self.peek(0) {
            if is_delimiter(c) {
                break;
            }
            match c {
                '\'' => {
                    self.i += 1;
                    let begin = self.i;
                    while self.peek(0).map_or(false, |c| c != '\'') {
                        self.i += 1;
                    }
                    if self.peek(0).is_none() {
                        return Err(ParseError::Incomplete);
                    }
                    let text: String = self.cs[begin..self.i].iter().collect();
                    self.i += 1;
                    b.quoted_lit(&text);
                }
                '"' => {
                    self.i += 1;
                    self.double_quoted(&mut b)?;
                }
                '\\' => match self.peek(1) {
                    None => return Err(ParseError::Incomplete),
                    Some('\n') => self.i += 2,
                    Some(n) => {
                        b.quoted_lit(&String::from(n));
                        self.i += 2;
                    }
                },
                '$' => {
                    let part = self.dollar(false)?;
                    b.push(part, false);
                }
                '`' => {
                    let text = self.backquoted()?;
                    b.push(Part::CmdSub(text), false);
                }
                '~' if self.i == start && self.peek(1).map_or(true, |n| n == '/' || is_delimiter(n)) => {
                    self.i += 1;
                    b.push(Part::Tilde, false);
                }
                other => {
                    b.lit_char(other);
                    self.i += 1;
                }
            }
        }
        Ok(b.finish())
    }

    /// After an opening `"`: read to the closing one.
    fn double_quoted(&mut self, b: &mut WordBuilder) -> Result<(), ParseError> {
        // An empty "" must still count as a word, but "$@" with no parameters must not: the empty
        // quoted text is added only if nothing at all came out of the quotes.
        let before = (b.parts.len(), b.lit.len());
        loop {
            let Some(c) = self.peek(0) else { return Err(ParseError::Incomplete) };
            match c {
                '"' => {
                    self.i += 1;
                    if (b.parts.len(), b.lit.len()) == before {
                        b.quoted_lit("");
                    }
                    return Ok(());
                }
                '\\' => match self.peek(1) {
                    None => return Err(ParseError::Incomplete),
                    Some('\n') => self.i += 2,
                    Some(n @ ('$' | '"' | '\\' | '`')) => {
                        b.quoted_lit(&String::from(n));
                        self.i += 2;
                    }
                    Some(_) => {
                        b.quoted_lit("\\");
                        self.i += 1;
                    }
                },
                '$' => {
                    let part = self.dollar(true)?;
                    b.push(part, true);
                }
                '`' => {
                    let text = self.backquoted()?;
                    b.push(Part::CmdSub(text), true);
                }
                other => {
                    b.quoted_lit(&String::from(other));
                    self.i += 1;
                }
            }
        }
    }

    /// The text of a backquoted command; the cursor is on the opening backquote.
    fn backquoted(&mut self) -> Result<String, ParseError> {
        self.i += 1;
        let mut text = String::new();
        loop {
            let Some(c) = self.peek(0) else { return Err(ParseError::Incomplete) };
            self.i += 1;
            match c {
                '`' => return Ok(text),
                '\\' => match self.peek(0) {
                    Some(n @ ('`' | '\\' | '$')) => {
                        text.push(n);
                        self.i += 1;
                    }
                    _ => text.push('\\'),
                },
                other => text.push(other),
            }
        }
    }

    /// The cursor is on a `$`. In double quotes (`quoted`), `$` before something that cannot
    /// start an expansion is just a dollar sign.
    fn dollar(&mut self, quoted: bool) -> Result<Part, ParseError> {
        let _ = quoted;
        match self.peek(1) {
            Some('(') => {
                if self.peek(2) == Some('(') {
                    // $(( arithmetic ))
                    let text = self.balanced(2, '(', ')')?;
                    let inner = text.strip_suffix(')').unwrap_or(&text);
                    return Ok(Part::Arith(String::from(inner)));
                }
                let text = self.balanced(1, '(', ')')?;
                Ok(Part::CmdSub(text))
            }
            Some('{') => {
                let inner = self.balanced(1, '{', '}')?;
                parse_braced(&inner)
            }
            Some(c) if is_name_start(c) => {
                self.i += 1;
                let begin = self.i;
                while self.peek(0).map_or(false, is_name_char) {
                    self.i += 1;
                }
                let name: String = self.cs[begin..self.i].iter().collect();
                Ok(Part::Var { name, op: VarOp::Plain })
            }
            Some(c) if c.is_ascii_digit() => {
                self.i += 2;
                Ok(Part::Var { name: String::from(c), op: VarOp::Plain })
            }
            Some(c) if matches!(c, '?' | '$' | '#' | '@' | '*' | '!' | '-') => {
                self.i += 2;
                Ok(Part::Var { name: String::from(c), op: VarOp::Plain })
            }
            _ => {
                self.i += 1;
                Ok(Part::Lit(String::from("$")))
            }
        }
    }

    /// The text between a matched pair of brackets. The cursor is on the `$`; `skip` is how many
    /// characters (`$(`, `$((`) come before the content. Quotes inside are respected. Leaves the
    /// cursor after the closing bracket. For `$((` the returned text ends with the first `)` of
    /// the closing pair.
    fn balanced(&mut self, skip: usize, open: char, close: char) -> Result<String, ParseError> {
        self.i += 1; // the $
        let mut depth = 0i32;
        let mut text = String::new();
        let mut opened = 0;
        // Consume the opening brackets.
        while opened < skip {
            let c = self.peek(0).ok_or(ParseError::Incomplete)?;
            if c == open {
                depth += 1;
                opened += 1;
                self.i += 1;
            } else {
                return syntax("bad expansion");
            }
        }
        loop {
            let c = self.peek(0).ok_or(ParseError::Incomplete)?;
            self.i += 1;
            if c == '\'' && open == '(' {
                text.push(c);
                loop {
                    let q = self.peek(0).ok_or(ParseError::Incomplete)?;
                    self.i += 1;
                    text.push(q);
                    if q == '\'' {
                        break;
                    }
                }
                continue;
            }
            if c == '"' && open == '(' {
                text.push(c);
                loop {
                    let q = self.peek(0).ok_or(ParseError::Incomplete)?;
                    self.i += 1;
                    text.push(q);
                    if q == '\\' {
                        if let Some(n) = self.peek(0) {
                            text.push(n);
                            self.i += 1;
                        }
                    } else if q == '"' {
                        break;
                    }
                }
                continue;
            }
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    // For $(( ... )) the first ")" of the closing pair is in `text` already;
                    // the caller strips it.
                    return Ok(text);
                }
            }
            text.push(c);
        }
    }
}

/// `${...}`: the text between the braces.
fn parse_braced(inner: &str) -> Result<Part, ParseError> {
    if let Some(name) = inner.strip_prefix('#') {
        if !name.is_empty() && name.chars().all(|c| is_name_char(c) || matches!(c, '?' | '#' | '@' | '*' | '$')) {
            return Ok(Part::Var { name: String::from(name), op: VarOp::Length });
        }
    }
    let cs: Vec<char> = inner.chars().collect();
    let mut n = 0;
    if cs.first().map_or(false, |c| matches!(c, '?' | '$' | '#' | '@' | '*' | '!')) {
        n = 1;
    } else if cs.first().map_or(false, |c| c.is_ascii_digit()) {
        while cs.get(n).map_or(false, |c| c.is_ascii_digit()) {
            n += 1;
        }
    } else {
        while cs.get(n).map_or(false, |&c| is_name_char(c)) {
            n += 1;
        }
    }
    if n == 0 {
        return syntax("bad substitution");
    }
    let name: String = cs[..n].iter().collect();
    let rest: String = cs[n..].iter().collect();
    if rest.is_empty() {
        return Ok(Part::Var { name, op: VarOp::Plain });
    }
    let (colon, body) = match rest.strip_prefix(':') {
        Some(b) => (true, b),
        None => (false, rest.as_str()),
    };
    let mut chars = body.chars();
    let kind = chars.next();
    let word_text: String = chars.collect();
    let word = parse_inner_word(&word_text)?;
    let op = match kind {
        Some('-') => VarOp::Default(word, colon),
        Some('+') => VarOp::Alternative(word, colon),
        Some('=') => VarOp::Assign(word, colon),
        _ => return syntax("unsupported ${...} form"),
    };
    Ok(Part::Var { name, op })
}

/// The word inside `${name:-word}`: expansions and quotes work, blanks do not end it.
fn parse_inner_word(text: &str) -> Result<Word, ParseError> {
    let mut lx = Lexer::new(text);
    let mut b = WordBuilder::default();
    while let Some(c) = lx.peek(0) {
        match c {
            '\'' => {
                lx.i += 1;
                let begin = lx.i;
                while lx.peek(0).map_or(false, |c| c != '\'') {
                    lx.i += 1;
                }
                let t: String = lx.cs[begin..lx.i].iter().collect();
                lx.i += 1;
                b.quoted_lit(&t);
            }
            '"' => {
                lx.i += 1;
                lx.double_quoted(&mut b)?;
            }
            '\\' => {
                if let Some(n) = lx.peek(1) {
                    b.quoted_lit(&String::from(n));
                }
                lx.i += 2;
            }
            '$' => {
                let part = lx.dollar(false)?;
                b.push(part, false);
            }
            '`' => {
                let t = lx.backquoted()?;
                b.push(Part::CmdSub(t), false);
            }
            other => {
                b.lit_char(other);
                lx.i += 1;
            }
        }
    }
    Ok(b.finish())
}

#[derive(Default)]
struct WordBuilder {
    parts: Vec<WordPart>,
    /// Unquoted literal text collected so far.
    lit: String,
}

impl WordBuilder {
    fn flush(&mut self) {
        if !self.lit.is_empty() {
            let text = core::mem::take(&mut self.lit);
            self.parts.push(WordPart { part: Part::Lit(text), quoted: false });
        }
    }

    fn lit_char(&mut self, c: char) {
        self.lit.push(c);
    }

    fn quoted_lit(&mut self, s: &str) {
        self.flush();
        // Merge with a quoted literal just before, so that "ab" is one part.
        if let Some(WordPart { part: Part::Lit(prev), quoted: true }) = self.parts.last_mut() {
            prev.push_str(s);
        } else {
            self.parts.push(WordPart { part: Part::Lit(String::from(s)), quoted: true });
        }
    }

    fn push(&mut self, part: Part, quoted: bool) {
        self.flush();
        self.parts.push(WordPart { part, quoted });
    }

    fn finish(mut self) -> Word {
        self.flush();
        Word(self.parts)
    }
}

/// Split text into tokens.
fn tokenize(text: &str) -> Result<Vec<Tok>, ParseError> {
    Lexer::new(text).tokens()
}

// ---- the parser ------------------------------------------------------------------------------------------

const RESERVED: &[&str] = &["if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case", "esac", "{", "}", "!"];

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

/// Parse a whole script or command line.
pub fn parse(text: &str) -> Result<List, ParseError> {
    let toks = tokenize(text)?;
    let mut p = Parser { toks, pos: 0 };
    let list = p.list(&[])?;
    if p.pos < p.toks.len() {
        return match &p.toks[p.pos] {
            Tok::Op(Op::RParen) => syntax("unexpected ')'"),
            Tok::Op(Op::DSemi) => syntax("unexpected ';;'"),
            Tok::Word(w) => match w.as_plain() {
                Some(k) if RESERVED.contains(&k) => Err(ParseError::Syntax(alloc::format!("unexpected '{}'", k))),
                _ => syntax("unexpected word"),
            },
            _ => syntax("unexpected token"),
        };
    }
    Ok(list)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// The reserved word at the cursor, if the next token is one.
    fn reserved(&self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Word(w)) => w.as_plain().filter(|s| RESERVED.contains(s)),
            _ => None,
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.pos += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline) | Some(Tok::Op(Op::Semi))) {
            self.pos += 1;
        }
    }

    fn expect_reserved(&mut self, kw: &str) -> Result<(), ParseError> {
        match self.reserved() {
            Some(k) if k == kw => {
                self.pos += 1;
                Ok(())
            }
            _ if self.peek().is_none() => Err(ParseError::Incomplete),
            _ => Err(ParseError::Syntax(alloc::format!("expected '{}'", kw))),
        }
    }

    /// Commands up to (not including) one of the `stop` words, or the end of the input.
    fn list(&mut self, stop: &[&str]) -> Result<List, ParseError> {
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            if self.peek().is_none() {
                return Ok(items);
            }
            if let Some(k) = self.reserved() {
                if stop.contains(&k) {
                    return Ok(items);
                }
            }
            if matches!(self.peek(), Some(Tok::Op(Op::RParen)) | Some(Tok::Op(Op::DSemi))) {
                return Ok(items);
            }
            let and_or = self.and_or()?;
            let mut background = false;
            match self.peek() {
                Some(Tok::Op(Op::Amp)) => {
                    background = true;
                    self.pos += 1;
                }
                Some(Tok::Op(Op::Semi)) | Some(Tok::Newline) | None => {}
                _ => {
                    // Something that is not a separator follows a complete command: it must be a
                    // closing word the caller wants, or an error.
                    let k = self.reserved();
                    if !k.map_or(false, |k| stop.contains(&k)) && !matches!(self.peek(), Some(Tok::Op(Op::RParen))) {
                        return syntax("unexpected token after a command");
                    }
                }
            }
            items.push(Item { and_or, background });
        }
    }

    fn and_or(&mut self) -> Result<AndOr, ParseError> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let conn = match self.peek() {
                Some(Tok::Op(Op::AndIf)) => Connector::And,
                Some(Tok::Op(Op::OrIf)) => Connector::Or,
                _ => return Ok(AndOr { first, rest }),
            };
            self.pos += 1;
            self.skip_newlines();
            if self.peek().is_none() {
                return Err(ParseError::Incomplete);
            }
            rest.push((conn, self.pipeline()?));
        }
    }

    fn pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut negate = false;
        if self.reserved() == Some("!") {
            negate = true;
            self.pos += 1;
        }
        let mut commands = alloc::vec![self.command()?];
        while matches!(self.peek(), Some(Tok::Op(Op::Pipe))) {
            self.pos += 1;
            self.skip_newlines();
            if self.peek().is_none() {
                return Err(ParseError::Incomplete);
            }
            commands.push(self.command()?);
        }
        Ok(Pipeline { negate, commands })
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        let command = self.command_inner()?;
        if matches!(command, Command::Simple { .. } | Command::Function { .. }) {
            return Ok(command);
        }
        // Redirections after a compound command apply to the whole of it.
        let mut redirs = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::IoNumber(n)) => {
                    let n = *n;
                    self.pos += 1;
                    redirs.push(self.redirection(Some(n))?);
                }
                Some(Tok::Op(Op::Less | Op::Great | Op::DGreat | Op::GreatAnd | Op::AndGreat)) => {
                    redirs.push(self.redirection(None)?);
                }
                _ => break,
            }
        }
        if redirs.is_empty() {
            Ok(command)
        } else {
            Ok(Command::Redirected { command: Box::new(command), redirs })
        }
    }

    fn command_inner(&mut self) -> Result<Command, ParseError> {
        if let Some(k) = self.reserved() {
            match k {
                "if" => return self.if_command(),
                "while" => return self.while_command(false),
                "until" => return self.while_command(true),
                "for" => return self.for_command(),
                "{" => {
                    self.pos += 1;
                    let body = self.list(&["}"])?;
                    self.expect_reserved("}")?;
                    return Ok(Command::Group(body));
                }
                "case" => return syntax("'case' is not supported"),
                "!" => {}
                other if other != "!" => {
                    return Err(ParseError::Syntax(alloc::format!("unexpected '{}'", other)));
                }
                _ => {}
            }
        }
        if matches!(self.peek(), Some(Tok::Op(Op::LParen))) {
            return syntax("subshells ( ... ) are not supported; use { ...; }");
        }
        // name() { ... }
        if let (Some(Tok::Word(w)), Some(Tok::Op(Op::LParen)), Some(Tok::Op(Op::RParen))) =
            (self.toks.get(self.pos), self.toks.get(self.pos + 1), self.toks.get(self.pos + 2))
        {
            if let Some(name) = w.as_plain() {
                if name.chars().all(is_name_char) && !name.is_empty() {
                    let name = String::from(name);
                    self.pos += 3;
                    self.skip_newlines();
                    if self.peek().is_none() {
                        return Err(ParseError::Incomplete);
                    }
                    let body = self.command()?;
                    return Ok(Command::Function { name, body: Box::new(body) });
                }
            }
        }
        self.simple_command()
    }

    fn if_command(&mut self) -> Result<Command, ParseError> {
        self.pos += 1; // if
        let mut arms = Vec::new();
        let mut otherwise = None;
        let cond = self.list(&["then"])?;
        self.expect_reserved("then")?;
        let body = self.list(&["elif", "else", "fi"])?;
        arms.push((cond, body));
        loop {
            match self.reserved() {
                Some("elif") => {
                    self.pos += 1;
                    let cond = self.list(&["then"])?;
                    self.expect_reserved("then")?;
                    let body = self.list(&["elif", "else", "fi"])?;
                    arms.push((cond, body));
                }
                Some("else") => {
                    self.pos += 1;
                    otherwise = Some(self.list(&["fi"])?);
                }
                Some("fi") => {
                    self.pos += 1;
                    return Ok(Command::If { arms, otherwise });
                }
                _ if self.peek().is_none() => return Err(ParseError::Incomplete),
                _ => return syntax("expected 'fi'"),
            }
        }
    }

    fn while_command(&mut self, until: bool) -> Result<Command, ParseError> {
        self.pos += 1;
        let cond = self.list(&["do"])?;
        self.expect_reserved("do")?;
        let body = self.list(&["done"])?;
        self.expect_reserved("done")?;
        Ok(Command::While { cond, body, until })
    }

    fn for_command(&mut self) -> Result<Command, ParseError> {
        self.pos += 1;
        let var = match self.peek() {
            Some(Tok::Word(w)) => match w.as_plain() {
                Some(n) if !n.is_empty() && n.chars().all(is_name_char) && !n.starts_with(|c: char| c.is_ascii_digit()) => String::from(n),
                _ => return syntax("bad name after 'for'"),
            },
            None => return Err(ParseError::Incomplete),
            _ => return syntax("expected a name after 'for'"),
        };
        self.pos += 1;
        self.skip_newlines();
        let mut items = None;
        if self.reserved() == Some("in") {
            self.pos += 1;
            let mut words = Vec::new();
            while let Some(Tok::Word(w)) = self.peek() {
                words.push(w.clone());
                self.pos += 1;
            }
            items = Some(words);
        }
        self.skip_separators();
        self.expect_reserved("do")?;
        let body = self.list(&["done"])?;
        self.expect_reserved("done")?;
        Ok(Command::For { var, items, body })
    }

    fn simple_command(&mut self) -> Result<Command, ParseError> {
        let mut assigns = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirs = Vec::new();
        loop {
            match self.peek().cloned() {
                Some(Tok::Word(w)) => {
                    if words.is_empty() {
                        if let Some((name, value)) = split_assignment(&w) {
                            assigns.push((name, value));
                            self.pos += 1;
                            continue;
                        }
                    }
                    words.push(w);
                    self.pos += 1;
                }
                Some(Tok::IoNumber(n)) => {
                    self.pos += 1;
                    redirs.push(self.redirection(Some(n))?);
                }
                Some(Tok::Op(Op::Less)) | Some(Tok::Op(Op::Great)) | Some(Tok::Op(Op::DGreat)) | Some(Tok::Op(Op::GreatAnd)) | Some(Tok::Op(Op::AndGreat)) => {
                    redirs.push(self.redirection(None)?);
                }
                _ => break,
            }
        }
        if words.is_empty() && assigns.is_empty() && redirs.is_empty() {
            return match self.peek() {
                None => Err(ParseError::Incomplete),
                Some(Tok::Op(Op::Semi)) | Some(Tok::Op(Op::Amp)) | Some(Tok::Newline) | Some(Tok::Op(Op::Pipe)) => {
                    syntax("missing command")
                }
                _ => syntax("unexpected token"),
            };
        }
        Ok(Command::Simple { assigns, words, redirs })
    }

    /// The operator (and its target) at the cursor; `fd` is an explicit descriptor number.
    fn redirection(&mut self, fd: Option<u32>) -> Result<Redir, ParseError> {
        let op = match self.peek() {
            Some(Tok::Op(op)) => *op,
            _ => return syntax("expected a redirection operator"),
        };
        self.pos += 1;
        let target = match self.peek() {
            Some(Tok::Word(w)) => w.clone(),
            None => return Err(ParseError::Incomplete),
            _ => return syntax("expected a file name after the redirection"),
        };
        self.pos += 1;
        let (default_fd, op) = match op {
            Op::Less => (0, RedirOp::In),
            Op::Great => (1, RedirOp::Out),
            Op::DGreat => (1, RedirOp::Append),
            Op::GreatAnd => (1, RedirOp::DupOut),
            Op::AndGreat => (1, RedirOp::OutAndErr),
            _ => return syntax("expected a redirection operator"),
        };
        Ok(Redir { fd: fd.unwrap_or(default_fd), op, target })
    }
}

/// `name=value` as the first word of a command: the variable and the value's word.
fn split_assignment(w: &Word) -> Option<(String, Word)> {
    let first = w.0.first()?;
    let Part::Lit(text) = &first.part else { return None };
    if first.quoted {
        return None;
    }
    let eq = text.find('=')?;
    let name = &text[..eq];
    if name.is_empty() || !name.chars().next().map_or(false, is_name_start) || !name.chars().all(is_name_char) {
        return None;
    }
    let mut value = Word::default();
    let after = &text[eq + 1..];
    if !after.is_empty() {
        value.0.push(WordPart { part: Part::Lit(String::from(after)), quoted: false });
    }
    value.0.extend(w.0[1..].iter().cloned());
    Some((String::from(name), value))
}

// ---- tests ---------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn words_of(cmd: &str) -> Vec<Vec<WordPart>> {
        let list = parse(cmd).unwrap();
        match &list[0].and_or.first.commands[0] {
            Command::Simple { words, .. } => words.iter().map(|w| w.0.clone()).collect(),
            other => panic!("not a simple command: {other:?}"),
        }
    }

    fn lit(s: &str) -> WordPart {
        WordPart { part: Part::Lit(String::from(s)), quoted: false }
    }

    fn qlit(s: &str) -> WordPart {
        WordPart { part: Part::Lit(String::from(s)), quoted: true }
    }

    fn var(name: &str, quoted: bool) -> WordPart {
        WordPart { part: Part::Var { name: String::from(name), op: VarOp::Plain }, quoted }
    }

    #[test]
    fn words_and_blanks() {
        assert_eq!(words_of("echo  hello\tworld"), [[lit("echo")], [lit("hello")], [lit("world")]]);
        assert_eq!(words_of("  ls  "), [[lit("ls")]]);
    }

    #[test]
    fn quoting_rules() {
        assert_eq!(words_of("echo 'a  b'"), [[lit("echo")], [qlit("a  b")]]);
        assert_eq!(words_of("echo \"a  b\""), [[lit("echo")], [qlit("a  b")]]);
        // Adjacent pieces form one word.
        let w = &words_of("echo a'b c'd")[1];
        assert_eq!(w, &[lit("a"), qlit("b c"), lit("d")]);
        // A backslash quotes the next character.
        assert_eq!(words_of("echo a\\ b"), vec![vec![lit("echo")], vec![lit("a"), qlit(" "), lit("b")]]);
        // Inside double quotes only \$ \" \\ \` are escapes.
        let w = &words_of("echo \"x\\$y\\n\"")[1];
        assert_eq!(w, &[qlit("x$y\\n")]);
        // An empty quoted string is still a word.
        assert_eq!(words_of("echo ''").len(), 2);
        assert_eq!(words_of("echo \"\"")[1], [qlit("")]);
    }

    #[test]
    fn unterminated_quotes_ask_for_more() {
        assert_eq!(parse("echo 'abc"), Err(ParseError::Incomplete));
        assert_eq!(parse("echo \"abc"), Err(ParseError::Incomplete));
        assert_eq!(parse("echo abc\\"), Err(ParseError::Incomplete));
        assert_eq!(parse("echo `ls"), Err(ParseError::Incomplete));
        assert_eq!(parse("echo $(ls"), Err(ParseError::Incomplete));
        assert_eq!(parse("echo ${x"), Err(ParseError::Incomplete));
    }

    #[test]
    fn variable_forms() {
        let w = &words_of("echo $HOME ${USER} $1 $? $$ $# $@ $* $9")[1..];
        let names: Vec<&str> = w
            .iter()
            .map(|p| match &p[0].part {
                Part::Var { name, op: VarOp::Plain } => name.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(names, ["HOME", "USER", "1", "?", "$", "#", "@", "*", "9"]);
        // A lone dollar sign, or one before a space, is just a dollar sign.
        assert_eq!(words_of("echo $ $ x")[1], [lit("$")]);
        assert_eq!(words_of("echo a$")[1], [lit("a"), lit("$")]);
        // Names end at the first character that cannot be in one.
        assert_eq!(words_of("echo $a.b")[1], [var("a", false), lit(".b")]);
        assert_eq!(words_of("echo ${a}b")[1], [var("a", false), lit("b")]);
    }

    #[test]
    fn braced_operators() {
        let part = |s: &str| words_of(&alloc::format!("echo {s}"))[1][0].part.clone();
        assert_eq!(part("${x:-def}"), Part::Var { name: "x".into(), op: VarOp::Default(Word::literal("def"), true) });
        assert_eq!(part("${x-def}"), Part::Var { name: "x".into(), op: VarOp::Default(Word::literal("def"), false) });
        assert_eq!(part("${x:+alt}"), Part::Var { name: "x".into(), op: VarOp::Alternative(Word::literal("alt"), true) });
        assert_eq!(part("${x:=v}"), Part::Var { name: "x".into(), op: VarOp::Assign(Word::literal("v"), true) });
        assert_eq!(part("${#x}"), Part::Var { name: "x".into(), op: VarOp::Length });
        assert_eq!(part("${#}"), Part::Var { name: "#".into(), op: VarOp::Plain });
        assert!(matches!(parse("echo ${}"), Err(ParseError::Syntax(_))));
        assert!(matches!(parse("echo ${x%y}"), Err(ParseError::Syntax(_))));
        // The default can itself contain expansions.
        let p = part("${x:-$y}");
        match p {
            Part::Var { op: VarOp::Default(w, true), .. } => assert_eq!(w.0, [var("y", false)]),
            other => panic!("{other:?}"),
        }
    }


    #[test]
    fn command_and_arithmetic_substitution() {
        assert_eq!(words_of("echo $(ls -l)")[1][0].part, Part::CmdSub("ls -l".into()));
        assert_eq!(words_of("echo `ls -l`")[1][0].part, Part::CmdSub("ls -l".into()));
        assert_eq!(words_of("echo $(echo $(echo hi))")[1][0].part, Part::CmdSub("echo $(echo hi)".into()));
        assert_eq!(words_of("echo $(echo ')' x)")[1][0].part, Part::CmdSub("echo ')' x".into()));
        assert_eq!(words_of("echo $((1 + 2))")[1][0].part, Part::Arith("1 + 2".into()));
        assert_eq!(words_of("echo $(( (1+2)*3 ))")[1][0].part, Part::Arith(" (1+2)*3 ".into()));
        assert!(words_of("echo \"$(date)\"")[1][0].quoted);
    }

    #[test]
    fn tilde_only_at_the_start_of_a_word() {
        assert_eq!(words_of("cd ~")[1][0].part, Part::Tilde);
        assert_eq!(words_of("ls ~/docs")[1][0].part, Part::Tilde);
        assert_eq!(words_of("echo a~b")[1], [lit("a~b")]);
        assert_eq!(words_of("echo '~'")[1], [qlit("~")]);
    }

    #[test]
    fn operators_and_lists() {
        let list = parse("a | b | c && d || e ; f &").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].and_or.first.commands.len(), 3);
        assert_eq!(list[0].and_or.rest.len(), 2);
        assert_eq!(list[0].and_or.rest[0].0, Connector::And);
        assert_eq!(list[0].and_or.rest[1].0, Connector::Or);
        assert!(!list[0].background);
        assert!(list[1].background);
        // Newlines separate commands too.
        assert_eq!(parse("a\nb\n\n c").unwrap().len(), 3);
        assert_eq!(parse("# only a comment").unwrap().len(), 0);
        assert_eq!(parse("a # trailing\nb").unwrap().len(), 2);
        assert_eq!(parse("").unwrap().len(), 0);
        // An operator at the end wants more.
        assert_eq!(parse("a |"), Err(ParseError::Incomplete));
        assert_eq!(parse("a &&"), Err(ParseError::Incomplete));
        assert_eq!(parse("a ||\n"), Err(ParseError::Incomplete));
        assert!(matches!(parse("| a"), Err(ParseError::Syntax(_))));
        assert!(matches!(parse("a ; ; b"), Ok(_)));
        assert!(matches!(parse("&& a"), Err(ParseError::Syntax(_))));
    }

    #[test]
    fn redirections() {
        let list = parse("cmd < in > out 2> err >> log 2>&1 &> both").unwrap();
        let Command::Simple { redirs, words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(words.len(), 1);
        let got: Vec<(u32, RedirOp)> = redirs.iter().map(|r| (r.fd, r.op)).collect();
        assert_eq!(
            got,
            [(0, RedirOp::In), (1, RedirOp::Out), (2, RedirOp::Out), (1, RedirOp::Append), (2, RedirOp::DupOut), (1, RedirOp::OutAndErr)]
        );
        // Redirections can come first, and a number that is not followed by an operator is a word.
        let list = parse("> out echo 2 3").unwrap();
        let Command::Simple { redirs, words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(redirs.len(), 1);
        assert_eq!(words.len(), 3);
        assert_eq!(parse("echo >"), Err(ParseError::Incomplete));
        assert!(matches!(parse("echo > |"), Err(ParseError::Syntax(_))));
        assert!(matches!(parse("cat << EOF"), Err(ParseError::Syntax(_))));
    }

    #[test]
    fn assignments() {
        let list = parse("A=1 B=\"x y\" cmd arg").unwrap();
        let Command::Simple { assigns, words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(assigns.len(), 2);
        assert_eq!(assigns[0].0, "A");
        assert_eq!(assigns[1].0, "B");
        assert_eq!(words.len(), 2);
        // After the command word, name=value is an ordinary argument.
        let list = parse("echo A=1").unwrap();
        let Command::Simple { assigns, words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        assert!(assigns.is_empty());
        assert_eq!(words.len(), 2);
        // Not assignments: bad names, quoted names.
        for text in ["1A=x", "=x", "'A'=x", "A-B=x", "A.B=c"] {
            let list = parse(text).unwrap();
            let Command::Simple { assigns, words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
            assert!(assigns.is_empty(), "{text}");
            assert_eq!(words.len(), 1, "{text}");
        }
        // A plain assignment of nothing.
        let list = parse("A=").unwrap();
        let Command::Simple { assigns, words, .. } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(assigns[0].1, Word::default());
        assert!(words.is_empty());
    }

    #[test]
    fn if_while_for() {
        let list = parse("if a; then b; elif c; then d; else e; fi").unwrap();
        let Command::If { arms, otherwise } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(arms.len(), 2);
        assert!(otherwise.is_some());
        assert_eq!(parse("if a\nthen\n  b\nfi\n").unwrap().len(), 1);
        assert_eq!(parse("if a; then b"), Err(ParseError::Incomplete));
        assert_eq!(parse("if a; then b; else"), Err(ParseError::Incomplete));
        assert!(matches!(parse("if a; then b; done"), Err(ParseError::Syntax(_))));
        assert!(matches!(parse("fi"), Err(ParseError::Syntax(_))));
        assert!(matches!(parse("then"), Err(ParseError::Syntax(_))));

        let list = parse("while a; do b; done").unwrap();
        assert!(matches!(&list[0].and_or.first.commands[0], Command::While { until: false, .. }));
        let list = parse("until a; do b; done").unwrap();
        assert!(matches!(&list[0].and_or.first.commands[0], Command::While { until: true, .. }));
        assert_eq!(parse("while a; do b;"), Err(ParseError::Incomplete));

        let list = parse("for x in 1 2 3; do echo $x; done").unwrap();
        let Command::For { var, items, body } = &list[0].and_or.first.commands[0] else { panic!() };
        assert_eq!(var, "x");
        assert_eq!(items.as_ref().unwrap().len(), 3);
        assert_eq!(body.len(), 1);
        let list = parse("for x do echo $x; done").unwrap();
        assert!(matches!(&list[0].and_or.first.commands[0], Command::For { items: None, .. }));
        assert_eq!(parse("for x in a b"), Err(ParseError::Incomplete));
        assert!(matches!(parse("for 1x in a; do b; done"), Err(ParseError::Syntax(_))));
    }

    #[test]
    fn keywords_are_ordinary_words_after_the_command_name() {
        let w = words_of("echo if then fi done");
        assert_eq!(w.len(), 5);
        // ... and when quoted.
        assert!(matches!(parse("'if' a").unwrap()[0].and_or.first.commands[0], Command::Simple { .. }));
    }

    #[test]
    fn groups_and_functions() {
        let list = parse("{ a; b; }").unwrap();
        assert!(matches!(&list[0].and_or.first.commands[0], Command::Group(l) if l.len() == 2));
        assert_eq!(parse("{ a;"), Err(ParseError::Incomplete));
        let list = parse("greet() { echo hi $1; }").unwrap();
        assert!(matches!(&list[0].and_or.first.commands[0], Command::Function { name, .. } if name == "greet"));
        let list = parse("f ()\n{\n echo x\n}\n").unwrap();
        assert!(matches!(&list[0].and_or.first.commands[0], Command::Function { .. }));
        assert!(matches!(parse("(a; b)"), Err(ParseError::Syntax(_))));
        assert!(matches!(parse("case x in x) a;; esac"), Err(ParseError::Syntax(_))));
    }

    #[test]
    fn negation() {
        let list = parse("! grep x file").unwrap();
        assert!(list[0].and_or.first.negate);
        let list = parse("echo !").unwrap();
        assert!(!list[0].and_or.first.negate);
    }

    #[test]
    fn nesting_works() {
        let text = "for f in a b; do if test -d $f; then echo dir; else echo file; fi; done";
        assert_eq!(parse(text).unwrap().len(), 1);
        let text = "while true; do\n  if x; then\n    break\n  fi\ndone";
        assert_eq!(parse(text).unwrap().len(), 1);
    }

    #[test]
    fn garbage_never_panics() {
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        let alphabet: Vec<char> = "ab ;|&<>()$`'\"\\{}#~*?=\n\t0123if then fi do done for in while".chars().collect();
        for _ in 0..30_000 {
            let len = (s % 30) as usize;
            let mut text = String::new();
            for _ in 0..len {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                text.push(alphabet[(s % alphabet.len() as u64) as usize]);
            }
            let _ = parse(&text);
        }
    }
}
