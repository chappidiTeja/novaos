//! sh: the NovaOS shell.
//!
//!   sh                    interactive, reading commands from the keyboard
//!   sh -c 'command'       run one command line
//!   sh script [args...]   run a script (a file of commands); $0 is its name, $1... the arguments
//!
//! The language is a practical subset of the usual Unix shell: pipelines, `&&` `||` `;` `&`,
//! redirections, quoting, variables and `$(command)` / `$((arithmetic))`, wildcards, `if`
//! `while` `until` `for`, functions, and job control with `&`, `jobs`, `wait`, `fg`, `kill`.
//! Type `help` for the list of built-in commands.

#![no_std]
#![no_main]

extern crate alloc;

mod builtins;
mod editor;
mod exec;
mod fsutil;
mod shell;

use alloc::string::String;
use alloc::vec::Vec;
use editor::{Completion, Editor, Line};
use nova::*;
use novashell::{parse, ParseError};
use shell::Shell;

/// First-word alias substitution (once, no recursion).
fn expand_alias(sh: &Shell, text: &str) -> String {
    let trimmed = text.trim_start();
    let first_end = trimmed.find(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&')).unwrap_or(trimmed.len());
    match sh.aliases.get(&trimmed[..first_end]) {
        Some(replacement) => alloc::format!("{}{}", replacement, &trimmed[first_end..]),
        None => String::from(text),
    }
}

/// Candidates for the word being typed at `cursor`.
fn complete(sh: &Shell, line: &str, cursor: usize) -> Completion {
    let before = &line[..cursor];
    let start = before.rfind(|c: char| matches!(c, ' ' | '\t' | '|' | ';' | '&' | '(' | ')' | '<' | '>')).map_or(0, |i| i + 1);
    let word = &before[start..];
    let prefix = before[..start].trim_end();
    let command_position = prefix.is_empty() || prefix.ends_with(|c: char| matches!(c, '|' | ';' | '&' | '(' | '{'));

    let mut names: Vec<String> = Vec::new();
    if let Some(var) = word.strip_prefix('$') {
        for k in sh.vars.keys() {
            if k.starts_with(var) {
                names.push(alloc::format!("${}", k));
            }
        }
    } else if command_position && !word.contains('/') {
        for b in builtins::builtin_names() {
            if b.starts_with(word) {
                names.push(String::from(*b));
            }
        }
        for f in sh.funcs.keys().chain(sh.aliases.keys()) {
            if f.starts_with(word) {
                names.push(f.clone());
            }
        }
        for dir in sh.path_dirs() {
            for n in fsutil::list_dir(&dir) {
                if n.starts_with(word) {
                    names.push(n);
                }
            }
        }
    } else {
        // File names. Everything up to the last slash is the directory to look in.
        let (dir, base) = match word.rfind('/') {
            Some(i) => (&word[..=i], &word[i + 1..]),
            None => ("", word),
        };
        let look_in = if dir.is_empty() { "" } else { dir };
        for n in fsutil::list_dir(look_in) {
            if n.starts_with(base) && (base.starts_with('.') || !n.starts_with('.')) {
                let full = alloc::format!("{}{}", dir, n);
                names.push(if fsutil::is_dir(&full) { alloc::format!("{}/", full) } else { full });
            }
        }
    }
    names.sort();
    names.dedup();
    Completion { start, candidates: names }
}

fn interactive(sh: &mut Shell) -> i32 {
    // The shell must survive Control-C itself; the programs it starts do not inherit this.
    sigignore(1 << SIGINT);
    println!("NovaOS shell. Type 'help' for commands.");
    let mut editor = Editor::new();
    loop {
        sh.reap_jobs();
        if let Some(code) = sh.exit {
            return code;
        }
        let prompt = alloc::format!("nova:{}$ ", fsutil::cwd());
        print!("{}", prompt);
        let mut text = String::new();
        let mut give_up = false;
        loop {
            let line = {
                let shr: &Shell = sh;
                let p = if text.is_empty() { prompt.as_str() } else { "> " };
                editor.read_line(p, &shr.history, &mut |l, c| complete(shr, l, c))
            };
            match line {
                Line::Eof => {
                    if text.is_empty() {
                        println!("exit");
                        return sh.exit.unwrap_or(sh.status);
                    }
                    sh.complain("syntax error: unexpected end of input");
                    give_up = true;
                    break;
                }
                Line::Interrupted => {
                    give_up = true;
                    break;
                }
                Line::Text(l) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&l);
                    match parse(&expand_alias(sh, &text)) {
                        Err(ParseError::Incomplete) => {
                            print!("> ");
                            continue;
                        }
                        _ => break,
                    }
                }
            }
        }
        if give_up || text.trim().is_empty() {
            continue;
        }
        if sh.history.last().map_or(true, |last| *last != text) {
            sh.history.push(text.clone());
            if sh.history.len() > 200 {
                sh.history.remove(0);
            }
        }
        let expanded = expand_alias(sh, &text);
        sh.run_text(&expanded);
    }
}

fn main(args: &Args) -> i32 {
    // sh -c 'command' [name [args...]]
    if args.get(1) == "-c" {
        let mut sh = Shell::new(args, false);
        if args.len() < 3 {
            eprintln!("sh: -c needs a command");
            return 2;
        }
        if args.len() > 3 {
            sh.script_name = String::from(args.get(3));
            sh.params = (4..args.len()).map(|i| String::from(args.get(i))).collect();
        }
        let status = sh.run_text(args.get(2));
        return sh.exit.unwrap_or(status);
    }
    // sh script [args...]
    if args.len() > 1 {
        let mut sh = Shell::new(args, false);
        let path = args.get(1);
        let text = match fsutil::read_file(path) {
            Ok(t) => String::from_utf8_lossy(&t).into_owned(),
            Err(e) => {
                eprintln!("sh: {}: {}", path, strerror(e));
                return 127;
            }
        };
        sh.script_name = String::from(path);
        sh.params = (2..args.len()).map(|i| String::from(args.get(i))).collect();
        let status = sh.run_text(&text);
        return sh.exit.unwrap_or(status);
    }
    let mut sh = Shell::new(args, true);
    interactive(&mut sh)
}

entry!(main);
