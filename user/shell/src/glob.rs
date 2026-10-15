//! Filename patterns: `*` (any run of characters), `?` (any one), `[abc]`, `[a-z]`, `[!abc]` and
//! `\x` (the character x). Matching is on bytes; `/` is not special here (the caller matches one
//! path component at a time).

/// Does `text` match `pattern` in full?
pub fn matches(pattern: &str, text: &str) -> bool {
    match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn match_bytes(p: &[u8], t: &[u8]) -> bool {
    // Iterative matching with one backtrack point for the most recent `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                b'?' if ti < t.len() => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                b'[' if ti < t.len() => {
                    if let Some((ok, next)) = class(p, pi, t[ti]) {
                        if ok {
                            pi = next;
                            ti += 1;
                            continue;
                        }
                    } else if t[ti] == b'[' {
                        // An unterminated `[` is an ordinary character.
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() && ti < t.len() && p[pi + 1] == t[ti] => {
                    pi += 2;
                    ti += 1;
                    continue;
                }
                c if c != b'?' && c != b'[' && c != b'\\' && ti < t.len() && c == t[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        } else if ti == t.len() {
            return true;
        }
        // Mismatch: let the last `*` swallow one more character, if there is one.
        match star {
            Some((sp, st)) if st < t.len() => {
                star = Some((sp, st + 1));
                pi = sp + 1;
                ti = st + 1;
            }
            _ => return false,
        }
    }
}

/// Parse the `[...]` class starting at `p[at]` and test `c` against it. Returns (matched, index
/// just after the class), or None if the bracket is never closed.
fn class(p: &[u8], at: usize, c: u8) -> Option<(bool, usize)> {
    let mut i = at + 1;
    let negate = matches!(p.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let lo = *p.get(i)?;
        if lo == b']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        let mut lo = lo;
        if lo == b'\\' {
            i += 1;
            lo = *p.get(i)?;
        }
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).map_or(false, |&h| h != b']') {
            let hi = p[i + 2];
            if lo <= c && c <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if lo == c {
                matched = true;
            }
            i += 1;
        }
    }
}

/// Does the pattern contain anything that is not a plain character?
pub fn has_wildcards(pattern: &str) -> bool {
    let mut escaped = false;
    for b in pattern.bytes() {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if matches!(b, b'*' | b'?' | b'[') {
            return true;
        }
    }
    false
}

/// The pattern with its escapes removed: the literal text it stands for if it has no wildcards.
pub fn unescape(pattern: &str) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Escape every wildcard character in `text`, so that it matches only itself.
pub fn escape(text: &str) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_matches_itself() {
        assert!(matches("hello", "hello"));
        assert!(!matches("hello", "hell"));
        assert!(!matches("hello", "hello!"));
        assert!(matches("", ""));
        assert!(!matches("", "a"));
    }

    #[test]
    fn star_and_question_mark() {
        assert!(matches("*", ""));
        assert!(matches("*", "anything"));
        assert!(matches("*.txt", "notes.txt"));
        assert!(!matches("*.txt", "notes.txt.bak"));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(matches("a*b*c", "abc"));
        assert!(!matches("a*b*c", "acb"));
        assert!(matches("?", "x"));
        assert!(!matches("?", ""));
        assert!(!matches("?", "xy"));
        assert!(matches("f??.rs", "foo.rs"));
        assert!(matches("**a", "bbba"));
        // Backtracking: the first `*` must give up characters for the second part to match.
        assert!(matches("*ab*ab", "xabxabab"));
        assert!(!matches("*abc", "ababab"));
    }

    #[test]
    fn character_classes() {
        assert!(matches("[abc]x", "bx"));
        assert!(!matches("[abc]x", "dx"));
        assert!(matches("[a-c]", "b"));
        assert!(!matches("[a-c]", "d"));
        assert!(matches("[!a-c]", "d"));
        assert!(!matches("[!a-c]", "a"));
        assert!(matches("[^0-9]*", "abc"));
        assert!(matches("file[0-9][0-9]", "file42"));
        assert!(matches("[]]", "]"), "a ] right after [ is part of the class");
        assert!(matches("[a-]", "-"));
        assert!(matches("[[]", "["));
        // An unterminated class is an ordinary bracket.
        assert!(matches("[abc", "[abc"));
        assert!(!matches("[abc", "a"));
    }

    #[test]
    fn escapes_make_wildcards_literal() {
        assert!(matches("\\*", "*"));
        assert!(!matches("\\*", "x"));
        assert!(matches("a\\?b", "a?b"));
        assert!(matches("\\[x]", "[x]"));
        assert_eq!(unescape("a\\*b"), "a*b");
        assert_eq!(escape("a*b?[c]\\"), "a\\*b\\?\\[c\\]\\\\");
        for text in ["a*b", "[x]", "q?", "back\\slash", "plain"] {
            assert!(matches(&escape(text), text), "{text:?} must match its own escape");
        }
    }

    #[test]
    fn wildcard_detection() {
        assert!(has_wildcards("*.rs"));
        assert!(has_wildcards("a[0-9]"));
        assert!(has_wildcards("?"));
        assert!(!has_wildcards("plain"));
        assert!(!has_wildcards("escaped\\*star"));
    }

    #[test]
    fn pathological_patterns_finish_quickly() {
        // Exponential backtracking would take for ever on this.
        let text = "a".repeat(200);
        let pattern = "*a*a*a*a*a*a*a*a*a*a*b";
        assert!(!matches(pattern, &text));
    }
}
