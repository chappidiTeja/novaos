//! A small regular-expression engine for `grep`, `sed`-like tools and the editor.
//!
//! Syntax (the classic "basic/extended" common ground):
//!   `c`      the character c          `.`      any character
//!   `^` `$`  start / end of the line  `\c`     the character c literally (`\.` `\*` `\\`)
//!   `[abc]` `[a-z]` `[^0-9]`          a character class (`]` first, `-` first or last, are literal)
//!   `x*` `x+` `x?`                    zero or more, one or more, zero or one (greedy)
//!   `\d` `\w` `\s`                    digit, word character, white space (`\D` `\W` `\S` the opposite)
//!   `a|b`                             either (the whole pattern splits at top-level bars)
//! Not supported: groups `( )`, counted repeats `{n,m}`, back-references. A pattern that would
//! need more than a million matching steps is reported as "no match" rather than hanging.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq)]
enum Atom {
    Char(char),
    Any,
    Class { ranges: Vec<(char, char)>, negate: bool },
    Digit(bool),
    Word(bool),
    Space(bool),
    Begin,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Repeat {
    One,
    Star,
    Plus,
    Optional,
}

type Branch = Vec<(Atom, Repeat)>;

#[derive(Debug, Clone)]
pub struct Regex {
    branches: Vec<Branch>,
    ignore_case: bool,
}

const STEP_LIMIT: u32 = 1_000_000;

impl Regex {
    pub fn new(pattern: &str, ignore_case: bool) -> Result<Regex, String> {
        let mut branches = Vec::new();
        let mut current: Branch = Vec::new();
        let cs: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i];
            i += 1;
            let atom = match c {
                '|' => {
                    branches.push(core::mem::take(&mut current));
                    continue;
                }
                '.' => Atom::Any,
                '^' if current.is_empty() => Atom::Begin,
                '$' if i == cs.len() || cs[i] == '|' => Atom::End,
                '\\' => {
                    let Some(&n) = cs.get(i) else { return Err(String::from("pattern ends with a backslash")) };
                    i += 1;
                    match n {
                        'd' => Atom::Digit(true),
                        'D' => Atom::Digit(false),
                        'w' => Atom::Word(true),
                        'W' => Atom::Word(false),
                        's' => Atom::Space(true),
                        'S' => Atom::Space(false),
                        'n' => Atom::Char('\n'),
                        't' => Atom::Char('\t'),
                        other => Atom::Char(other),
                    }
                }
                '[' => {
                    let mut negate = false;
                    if cs.get(i) == Some(&'^') {
                        negate = true;
                        i += 1;
                    }
                    let mut ranges = Vec::new();
                    let mut first = true;
                    loop {
                        let Some(&lo) = cs.get(i) else { return Err(String::from("unterminated [")) };
                        i += 1;
                        if lo == ']' && !first {
                            break;
                        }
                        first = false;
                        if cs.get(i) == Some(&'-') && cs.get(i + 1).map_or(false, |&h| h != ']') {
                            let hi = cs[i + 1];
                            if hi < lo {
                                return Err(String::from("range out of order in []"));
                            }
                            ranges.push((lo, hi));
                            i += 2;
                        } else {
                            ranges.push((lo, lo));
                        }
                    }
                    Atom::Class { ranges, negate }
                }
                '*' | '+' | '?' if current.is_empty() => Atom::Char(c), // nothing to repeat: literal
                other => Atom::Char(other),
            };
            let repeat = if matches!(atom, Atom::Begin | Atom::End) {
                Repeat::One
            } else {
                match cs.get(i) {
                    Some('*') => Repeat::Star,
                    Some('+') => Repeat::Plus,
                    Some('?') => Repeat::Optional,
                    _ => Repeat::One,
                }
            };
            if repeat != Repeat::One {
                i += 1;
            }
            current.push((atom, repeat));
        }
        branches.push(current);
        Ok(Regex { branches, ignore_case })
    }

    /// Does the pattern match anywhere in `text` (a single line)?
    pub fn is_match(&self, text: &str) -> bool {
        self.find(text).is_some()
    }

    /// The first match: byte offsets (start, end).
    pub fn find(&self, text: &str) -> Option<(usize, usize)> {
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut steps = 0u32;
        for start in 0..=chars.len() {
            for branch in &self.branches {
                if let Some(end) = self.match_here(branch, &chars, start, text.len(), &mut steps) {
                    return Some((byte_at(&chars, start, text.len()), byte_at(&chars, end, text.len())));
                }
                if steps > STEP_LIMIT {
                    return None;
                }
            }
        }
        None
    }

    fn same(&self, a: char, b: char) -> bool {
        a == b || (self.ignore_case && a.to_lowercase().eq(b.to_lowercase()))
    }

    fn atom_matches(&self, atom: &Atom, c: char) -> bool {
        match atom {
            Atom::Char(x) => self.same(*x, c),
            Atom::Any => true,
            Atom::Class { ranges, negate } => {
                let inside = ranges.iter().any(|&(lo, hi)| {
                    (lo <= c && c <= hi)
                        || (self.ignore_case && {
                            let l = c.to_ascii_lowercase();
                            let u = c.to_ascii_uppercase();
                            (lo <= l && l <= hi) || (lo <= u && u <= hi)
                        })
                });
                inside != *negate
            }
            Atom::Digit(yes) => c.is_ascii_digit() == *yes,
            Atom::Word(yes) => (c.is_alphanumeric() || c == '_') == *yes,
            Atom::Space(yes) => c.is_whitespace() == *yes,
            Atom::Begin | Atom::End => false,
        }
    }

    /// Match `branch` starting at character index `at`; returns the character index after it.
    fn match_here(&self, branch: &[(Atom, Repeat)], chars: &[(usize, char)], at: usize, _len: usize, steps: &mut u32) -> Option<usize> {
        *steps += 1;
        if *steps > STEP_LIMIT {
            return None;
        }
        let Some(((atom, repeat), rest)) = branch.split_first() else { return Some(at) };
        match atom {
            Atom::Begin => {
                return if at == 0 { self.match_here(rest, chars, at, _len, steps) } else { None };
            }
            Atom::End => {
                return if at == chars.len() { self.match_here(rest, chars, at, _len, steps) } else { None };
            }
            _ => {}
        }
        let (min, max) = match repeat {
            Repeat::One => (1, 1),
            Repeat::Star => (0, usize::MAX),
            Repeat::Plus => (1, usize::MAX),
            Repeat::Optional => (0, 1),
        };
        // Greedy: take as many as allowed, then give back one at a time.
        let mut count = 0;
        while count < max && at + count < chars.len() && self.atom_matches(atom, chars[at + count].1) {
            count += 1;
        }
        loop {
            if count >= min {
                if let Some(end) = self.match_here(rest, chars, at + count, _len, steps) {
                    return Some(end);
                }
            }
            if count == 0 || count <= min {
                return None;
            }
            count -= 1;
            if *steps > STEP_LIMIT {
                return None;
            }
        }
    }
}

fn byte_at(chars: &[(usize, char)], index: usize, len: usize) -> usize {
    chars.get(index).map_or(len, |&(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> bool {
        Regex::new(pattern, false).unwrap().is_match(text)
    }

    fn found<'a>(pattern: &str, text: &'a str) -> Option<&'a str> {
        let (s, e) = Regex::new(pattern, false).unwrap().find(text)?;
        Some(&text[s..e])
    }

    #[test]
    fn literals_and_dot() {
        assert!(m("abc", "xxabcxx"));
        assert!(!m("abc", "xxabxcx"));
        assert!(m("a.c", "abc"));
        assert!(m("a.c", "a-c"));
        assert!(!m("a.c", "ac"));
        assert!(m("", "anything"));
        assert!(m("", ""));
    }

    #[test]
    fn anchors() {
        assert!(m("^abc", "abcdef"));
        assert!(!m("^abc", "xabc"));
        assert!(m("def$", "abcdef"));
        assert!(!m("def$", "defx"));
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "abcc"));
        assert!(m("^$", ""));
        assert!(!m("^$", "x"));
        // A ^ in the middle and a $ before more pattern are ordinary characters.
        assert!(m("a^b", "a^b"));
        assert!(m("a$b", "a$b"));
    }

    #[test]
    fn repeats() {
        assert!(m("ab*c", "ac"));
        assert!(m("ab*c", "abbbbc"));
        assert!(!m("ab+c", "ac"));
        assert!(m("ab+c", "abc"));
        assert!(m("ab?c", "ac"));
        assert!(m("ab?c", "abc"));
        assert!(!m("ab?c", "abbc"));
        assert!(m("a.*z", "a anything z"));
        assert!(m(".*", ""));
        assert_eq!(found("a+", "baaac"), Some("aaa"), "repeats are greedy");
        assert_eq!(found("a*", "baaac"), Some(""), "and the leftmost match can be empty");
        assert_eq!(found("b.*c", "abxcxc"), Some("bxcxc"));
        assert_eq!(found("x*y", "xxxy"), Some("xxxy"));
        // Backtracking: the greedy part must give characters back.
        assert!(m("a.*b.*c", "a--b--c"));
        assert_eq!(found("^.*b", "aabab"), Some("aabab"));
        assert_eq!(found("^.*b", "aabac"), Some("aab"));
    }

    #[test]
    fn classes() {
        assert!(m("[abc]x", "bx"));
        assert!(!m("[abc]x", "dx"));
        assert!(m("[a-c]+", "xxbxx"));
        assert!(m("[^0-9]", "a"));
        assert!(!m("^[^0-9]+$", "ab3"));
        assert!(m("[]x]", "]"));
        assert!(m("[a-]", "-"));
        assert!(m("[.]", "."), "a dot in a class is a dot");
        assert!(!m("[.]", "x"));
        assert_eq!(found("[0-9]+", "ab 123 cd"), Some("123"));
        assert!(Regex::new("[abc", false).is_err());
        assert!(Regex::new("[z-a]", false).is_err());
    }

    #[test]
    fn escapes_and_shorthands() {
        assert!(m("a\\.b", "a.b"));
        assert!(!m("a\\.b", "axb"));
        assert!(m("\\*", "2*3"));
        assert!(m("\\\\", "a\\b"));
        assert_eq!(found("\\d+", "ab12cd"), Some("12"));
        assert_eq!(found("\\w+", "  hello_1 "), Some("hello_1"));
        assert_eq!(found("\\s+", "ab  \tcd"), Some("  \t"));
        assert_eq!(found("\\D+", "12ab3"), Some("ab"));
        assert!(Regex::new("abc\\", false).is_err());
    }

    #[test]
    fn alternation() {
        assert!(m("cat|dog", "hotdog"));
        assert!(m("cat|dog", "concatenate"));
        assert!(!m("cat|dog", "bird"));
        assert!(m("^a|b$", "xxb"));
        assert!(m("^a|b$", "axx"));
        assert!(!m("^a|b$", "xax"));
        assert_eq!(found("a|ab", "ab"), Some("a"), "the first alternative that matches wins");
    }

    #[test]
    fn ignore_case() {
        let r = Regex::new("hello", true).unwrap();
        assert!(r.is_match("HeLLo world"));
        let r = Regex::new("[a-c]+", true).unwrap();
        assert_eq!(r.find("xxABCxx"), Some((2, 5)));
        let r = Regex::new("hello", false).unwrap();
        assert!(!r.is_match("HELLO"));
    }

    #[test]
    fn unicode_text_is_matched_by_characters() {
        assert_eq!(found("é+", "caféé!"), Some("éé"));
        assert!(m("^.$", "é"));
    }

    #[test]
    fn nothing_to_repeat_is_literal() {
        assert!(m("*a", "x*a"));
        assert!(m("+", "1+1"));
        assert!(m("?", "why?"));
    }

    #[test]
    fn pathological_patterns_give_up_instead_of_hanging() {
        let text = "a".repeat(60);
        let r = Regex::new("a*a*a*a*a*a*a*a*a*b", false).unwrap();
        assert!(!r.is_match(&text));
    }

    #[test]
    fn random_patterns_never_panic() {
        let mut s = 0x9E37_79B9u64;
        let alphabet: Vec<char> = "ab.*+?[]^$\\|-d w".chars().collect();
        for _ in 0..30_000 {
            let mut p = String::new();
            for _ in 0..(s % 10) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                p.push(alphabet[(s % alphabet.len() as u64) as usize]);
            }
            if let Ok(r) = Regex::new(&p, false) {
                let _ = r.is_match("aab ab.*+ 12");
            }
        }
    }
}
