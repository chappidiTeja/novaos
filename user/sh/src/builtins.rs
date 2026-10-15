//! Commands the shell runs itself: the ones that change the shell's own state (`cd`, `export`,
//! `exit`...), the ones scripts need for speed or convenience (`echo`, `printf`, `test`, `read`),
//! and job control.

use crate::fsutil;
use crate::shell::{Io, Shell};
use alloc::string::String;
use alloc::vec::Vec;
use nova::*;
use novashell::testexpr::{self, TestFs};

const NAMES: &[&str] = &[
    ":", "true", "false", "echo", "printf", "pwd", "cd", "exit", "return", "break", "continue", "export", "unset", "set",
    "shift", "env", "read", "test", "[", "source", ".", "eval", "type", "alias", "unalias", "jobs", "wait", "fg", "bg", "kill",
    "history", "help", "local", "trap", "exec",
];

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

pub fn builtin_names() -> &'static [&'static str] {
    NAMES
}

struct RealFs;

impl TestFs for RealFs {
    fn exists(&mut self, path: &str) -> bool {
        fsutil::exists(path)
    }
    fn is_file(&mut self, path: &str) -> bool {
        fsutil::is_file(path)
    }
    fn is_dir(&mut self, path: &str) -> bool {
        fsutil::is_dir(path)
    }
    fn size(&mut self, path: &str) -> Option<u64> {
        fsutil::file_size(path)
    }
}

fn out(io: &Io, text: &str) {
    write_all(io.fds[1], text.as_bytes());
}

fn err(io: &Io, text: &str) {
    write_all(io.fds[2], text.as_bytes());
}

fn parse_number(s: &str) -> Option<i64> {
    s.trim().parse().ok()
}

impl Shell {
    pub fn run_builtin(&mut self, name: &str, argv: &[String], io: &Io) -> i32 {
        let args = &argv[1..];
        match name {
            ":" | "true" | "trap" => 0,
            "false" => 1,
            "echo" => self.b_echo(args, io),
            "printf" => self.b_printf(args, io),
            "pwd" => {
                out(io, &alloc::format!("{}\n", fsutil::cwd()));
                0
            }
            "cd" => self.b_cd(args, io),
            "exit" => {
                let code = args.first().and_then(|a| parse_number(a)).map_or(self.status, |n| (n & 0xFF) as i32);
                self.exit = Some(code);
                code
            }
            "return" => {
                if self.func_depth == 0 {
                    err(io, "sh: return: can only return from a function\n");
                    return 1;
                }
                let code = args.first().and_then(|a| parse_number(a)).map_or(self.status, |n| (n & 0xFF) as i32);
                self.returning = Some(code);
                code
            }
            "break" | "continue" => {
                if self.loop_depth == 0 {
                    err(io, &alloc::format!("sh: {}: only meaningful in a loop\n", name));
                    return 1;
                }
                let n = args.first().and_then(|a| parse_number(a)).unwrap_or(1).max(1) as u32;
                let n = n.min(self.loop_depth);
                if name == "break" {
                    self.break_levels = n;
                } else {
                    self.continue_levels = n;
                }
                0
            }
            "export" => self.b_export(args, io),
            "local" => {
                for a in args {
                    match a.split_once('=') {
                        Some((k, v)) => self.set(k, v),
                        None => {
                            if self.get(a).is_none() {
                                self.set(a, "");
                            }
                        }
                    }
                }
                0
            }
            "unset" => {
                let funcs = args.first().map(|a| a == "-f").unwrap_or(false);
                for a in args.iter().filter(|a| !a.starts_with('-')) {
                    if funcs {
                        self.funcs.remove(a.as_str());
                    } else {
                        self.unset(a);
                    }
                }
                0
            }
            "set" => self.b_set(args, io),
            "shift" => {
                let n = args.first().and_then(|a| parse_number(a)).unwrap_or(1).max(0) as usize;
                if n > self.params.len() {
                    err(io, "sh: shift: not enough parameters\n");
                    return 1;
                }
                self.params.drain(..n);
                0
            }
            "env" => self.b_env(args, io),
            "read" => self.b_read(args, io),
            "test" => self.b_test(args, io, false),
            "[" => self.b_test(args, io, true),
            "source" | "." => self.b_source(args, io),
            "eval" => {
                let text = args.join(" ");
                self.run_text(&text)
            }
            "type" => self.b_type(args, io),
            "alias" => self.b_alias(args, io),
            "unalias" => {
                for a in args {
                    self.aliases.remove(a.as_str());
                }
                0
            }
            "jobs" => {
                for j in &self.jobs {
                    out(io, &alloc::format!("[{}]  Running  {}\n", j.id, j.text));
                }
                0
            }
            "wait" => self.b_wait(args, io),
            "fg" => self.b_fg(args, io),
            "bg" => {
                err(io, "sh: bg: jobs cannot be stopped, so there is nothing to continue\n");
                1
            }
            "kill" => self.b_kill(args, io),
            "history" => {
                for (i, line) in self.history.iter().enumerate() {
                    out(io, &alloc::format!("{:5}  {}\n", i + 1, line));
                }
                0
            }
            "help" => {
                out(io, HELP);
                0
            }
            "exec" => {
                err(io, "sh: exec: not supported (programs are started with spawn); just run the command\n");
                1
            }
            _ => 127,
        }
    }

    fn b_echo(&mut self, args: &[String], io: &Io) -> i32 {
        let mut newline = true;
        let mut escapes = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-n" => newline = false,
                "-e" => escapes = true,
                "-E" => escapes = false,
                "-ne" | "-en" => {
                    newline = false;
                    escapes = true;
                }
                _ => break,
            }
            i += 1;
        }
        let mut text = args[i..].join(" ");
        if escapes {
            let (t, stop) = unescape(&text);
            text = t;
            if stop {
                newline = false;
            }
        }
        if newline {
            text.push('\n');
        }
        out(io, &text);
        0
    }

    fn b_printf(&mut self, args: &[String], io: &Io) -> i32 {
        let Some(fmt) = args.first() else {
            err(io, "sh: printf: usage: printf format [arguments]\n");
            return 2;
        };
        let rest = &args[1..];
        let mut text = String::new();
        let mut used = 0;
        loop {
            let before = used;
            format_once(fmt, rest, &mut used, &mut text);
            // The format is reused while arguments are left, as long as it consumes some.
            if used >= rest.len() || used == before {
                break;
            }
        }
        out(io, &text);
        0
    }

    fn b_cd(&mut self, args: &[String], io: &Io) -> i32 {
        let target = match args.first().map(|s| s.as_str()) {
            None => self.home_dir(),
            Some("-") => match self.get("OLDPWD") {
                Some(old) => {
                    let old = String::from(old);
                    out(io, &alloc::format!("{}\n", old));
                    old
                }
                None => {
                    err(io, "sh: cd: OLDPWD not set\n");
                    return 1;
                }
            },
            Some(p) => String::from(p),
        };
        let before = fsutil::cwd();
        let r = chdir(&fsutil::cstr(&target));
        if r < 0 {
            err(io, &alloc::format!("sh: cd: {}: {}\n", target, strerror(r)));
            return 1;
        }
        let now = fsutil::cwd();
        self.set_exported("OLDPWD", &before);
        self.set_exported("PWD", &now);
        0
    }

    fn b_export(&mut self, args: &[String], io: &Io) -> i32 {
        let names: Vec<&String> = args.iter().filter(|a| a.as_str() != "-p").collect();
        if names.is_empty() {
            for (k, v) in &self.vars {
                if v.exported {
                    out(io, &alloc::format!("export {}={}\n", k, v.value));
                }
            }
            return 0;
        }
        for a in names {
            let (name, value) = match a.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (a.as_str(), None),
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                err(io, &alloc::format!("sh: export: '{}': not a valid name\n", a));
                return 1;
            }
            let v = value.map(String::from).or_else(|| self.get(name).map(String::from)).unwrap_or_default();
            self.set_exported(name, &v);
        }
        0
    }

    fn b_set(&mut self, args: &[String], io: &Io) -> i32 {
        if args.is_empty() {
            for (k, v) in &self.vars {
                out(io, &alloc::format!("{}={}\n", k, v.value));
            }
            return 0;
        }
        if args[0] == "--" {
            self.params = args[1..].to_vec();
            return 0;
        }
        if args[0].starts_with('-') || args[0].starts_with('+') {
            // Options such as -e and -x are accepted and ignored.
            return 0;
        }
        self.params = args.to_vec();
        0
    }

    fn b_env(&mut self, args: &[String], io: &Io) -> i32 {
        let split = args.iter().position(|a| !a.contains('=')).unwrap_or(args.len());
        let assigns: Vec<(String, String)> = args[..split]
            .iter()
            .filter_map(|a| a.split_once('=').map(|(k, v)| (String::from(k), String::from(v))))
            .collect();
        if split == args.len() {
            for (k, v) in &self.vars {
                if v.exported && !assigns.iter().any(|(ak, _)| ak == k) {
                    out(io, &alloc::format!("{}={}\n", k, v.value));
                }
            }
            for (k, v) in &assigns {
                out(io, &alloc::format!("{}={}\n", k, v));
            }
            return 0;
        }
        let mut group = crate::exec::Group::default();
        match self.run_words(&args[split..], &assigns, io, &mut group) {
            crate::exec::Outcome::Done(s) => s,
            crate::exec::Outcome::Started(pid) => self.wait_foreground(&[pid], group.pgid, None),
        }
    }

    fn b_read(&mut self, args: &[String], io: &Io) -> i32 {
        let names: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        let mut line = Vec::new();
        let mut got_any = false;
        let mut byte = [0u8; 1];
        loop {
            let n = read(io.fds[0], &mut byte);
            if n <= 0 {
                break;
            }
            got_any = true;
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        if !got_any {
            return 1;
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        if names.is_empty() {
            self.set("REPLY", &text);
            return 0;
        }
        let mut rest = text.trim_start();
        for (i, name) in names.iter().enumerate() {
            if i + 1 == names.len() {
                self.set(name, rest.trim_end());
            } else {
                let end = rest.find(|c: char| c == ' ' || c == '\t').unwrap_or(rest.len());
                self.set(name, &rest[..end]);
                rest = rest[end..].trim_start();
            }
        }
        0
    }

    fn b_test(&mut self, args: &[String], io: &Io, bracket: bool) -> i32 {
        let mut a: &[String] = args;
        if bracket {
            if a.last().map(|s| s.as_str()) != Some("]") {
                err(io, "sh: [: missing ']'\n");
                return 2;
            }
            a = &a[..a.len() - 1];
        }
        match testexpr::evaluate(a, &mut RealFs) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(e) => {
                err(io, &alloc::format!("sh: test: {}\n", e));
                2
            }
        }
    }

    fn b_source(&mut self, args: &[String], io: &Io) -> i32 {
        let Some(path) = args.first() else {
            err(io, "sh: source: filename argument required\n");
            return 2;
        };
        // Like a command, a bare name is looked up in PATH.
        let file = if path.contains('/') { Some(path.clone()) } else { self.find_program(path).or_else(|| Some(path.clone())) };
        let file = file.unwrap();
        let text = match fsutil::read_file(&file) {
            Ok(t) => String::from_utf8_lossy(&t).into_owned(),
            Err(e) => {
                err(io, &alloc::format!("sh: {}: {}\n", file, strerror(e)));
                return 1;
            }
        };
        // Extra arguments become the positional parameters for the script; without any, it
        // sees the caller's.
        let saved = (args.len() > 1).then(|| core::mem::replace(&mut self.params, args[1..].to_vec()));
        let status = self.run_text(&text);
        if let Some(saved) = saved {
            self.params = saved;
        }
        status
    }

    fn b_type(&mut self, args: &[String], io: &Io) -> i32 {
        let mut status = 0;
        for a in args {
            let name = a.as_str();
            if let Some(v) = self.aliases.get(name) {
                out(io, &alloc::format!("{} is aliased to '{}'\n", name, v));
            } else if self.funcs.contains_key(name) {
                out(io, &alloc::format!("{} is a function\n", name));
            } else if is_builtin(name) {
                out(io, &alloc::format!("{} is a shell builtin\n", name));
            } else if let Some(p) = self.find_program(name) {
                out(io, &alloc::format!("{} is {}\n", name, p));
            } else {
                err(io, &alloc::format!("sh: type: {}: not found\n", name));
                status = 1;
            }
        }
        status
    }

    fn b_alias(&mut self, args: &[String], io: &Io) -> i32 {
        if args.is_empty() {
            for (k, v) in &self.aliases {
                out(io, &alloc::format!("alias {}='{}'\n", k, v));
            }
            return 0;
        }
        let mut status = 0;
        for a in args {
            match a.split_once('=') {
                Some((k, v)) => {
                    self.aliases.insert(String::from(k), String::from(v));
                }
                None => match self.aliases.get(a.as_str()) {
                    Some(v) => out(io, &alloc::format!("alias {}='{}'\n", a, v)),
                    None => {
                        err(io, &alloc::format!("sh: alias: {}: not found\n", a));
                        status = 1;
                    }
                },
            }
        }
        status
    }

    /// `%n` or a process id: the job's index in the table, or the pid itself.
    fn job_index(&self, arg: &str) -> Option<usize> {
        if let Some(n) = arg.strip_prefix('%') {
            let id: u32 = n.parse().ok()?;
            return self.jobs.iter().position(|j| j.id == id);
        }
        let pid: u64 = arg.parse().ok()?;
        self.jobs.iter().position(|j| j.pids.contains(&pid))
    }

    fn b_wait(&mut self, args: &[String], io: &Io) -> i32 {
        if let Some(arg) = args.first() {
            let Some(at) = self.job_index(arg) else {
                err(io, &alloc::format!("sh: wait: {}: no such job\n", arg));
                return 127;
            };
            let job = self.jobs.remove(at);
            let mut status = 0;
            for pid in job.pids {
                status = self.wait_one(pid);
            }
            return status;
        }
        // Everything in the background.
        while !self.jobs.is_empty() {
            let mut raw = 0i64;
            let pid = wait(ANY_CHILD, 0, &mut raw);
            if pid <= 0 {
                break;
            }
            let pid = pid as u64;
            if let Some(at) = self.jobs.iter().position(|j| j.pids.contains(&pid)) {
                self.jobs[at].pids.retain(|&p| p != pid);
                if self.jobs[at].pids.is_empty() {
                    self.jobs.remove(at);
                }
            }
        }
        0
    }

    fn b_fg(&mut self, args: &[String], io: &Io) -> i32 {
        let at = match args.first() {
            Some(a) => self.job_index(a),
            None => self.jobs.len().checked_sub(1),
        };
        let Some(at) = at else {
            err(io, "sh: fg: no such job\n");
            return 1;
        };
        let job = self.jobs.remove(at);
        out(io, &alloc::format!("{}\n", job.text));
        self.wait_foreground(&job.pids, job.pgid, None)
    }

    fn b_kill(&mut self, args: &[String], io: &Io) -> i32 {
        if args.first().map(|s| s.as_str()) == Some("-l") {
            out(io, "HUP INT QUIT KILL USR1 USR2 ALRM TERM\n");
            return 0;
        }
        let mut sig = SIGTERM;
        let mut targets = args;
        if let Some(flag) = args.first().and_then(|a| a.strip_prefix('-')) {
            match signal_from_name(flag) {
                Some(s) => sig = s,
                None => {
                    err(io, &alloc::format!("sh: kill: unknown signal '{}'\n", flag));
                    return 1;
                }
            }
            targets = &args[1..];
        }
        if targets.is_empty() {
            err(io, "sh: kill: usage: kill [-SIGNAL] pid|%job...\n");
            return 2;
        }
        let mut status = 0;
        for t in targets {
            let r = if let Some(n) = t.strip_prefix('%') {
                match n.parse::<u32>().ok().and_then(|id| self.jobs.iter().find(|j| j.id == id)) {
                    Some(j) => kill_group(j.pgid, sig),
                    None => {
                        err(io, &alloc::format!("sh: kill: {}: no such job\n", t));
                        status = 1;
                        continue;
                    }
                }
            } else if let Ok(pid) = t.parse::<u64>() {
                kill(pid, sig)
            } else {
                err(io, &alloc::format!("sh: kill: {}: not a pid or job\n", t));
                status = 1;
                continue;
            };
            if r < 0 {
                err(io, &alloc::format!("sh: kill: {}: {}\n", t, strerror(r)));
                status = 1;
            }
        }
        status
    }
}

const HELP: &str = "NovaOS shell. Commands, pipes (a | b), redirection (> >> < 2> 2>&1), && || ; &, quoting,
$VAR ${VAR:-default} $(command) $((arithmetic)) ~ and wildcards (* ? [a-z]).
if/elif/else/fi, while/until/do/done, for x in ...; do ...; done, { ...; }, name() { ...; }.
Built in: cd pwd exit export unset set shift env echo printf read test [ source eval type
alias unalias jobs wait fg kill history help return break continue true false
Keys: Up/Down history, Left/Right Home/End, Tab completes, Ctrl-A/E/K/U/W/L, Ctrl-C cancels, Ctrl-D ends.
";

// ---- echo and printf helpers ------------------------------------------------------------------------------------

/// Process the backslash escapes `echo -e` knows. Returns the text and whether `\\c` cut it short.
fn unescape(s: &str) -> (String, bool) {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('f') => out.push('\x0c'),
            Some('e') => out.push('\x1b'),
            Some('v') => out.push('\x0b'),
            Some('\\') => out.push('\\'),
            Some('c') => return (out, true),
            Some('0') => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 && chars.peek().map_or(false, |d| ('0'..='7').contains(d)) {
                    v = v * 8 + chars.next().unwrap().to_digit(8).unwrap();
                    n += 1;
                }
                out.push(char::from_u32(v & 0xFF).unwrap_or('?'));
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    (out, false)
}

/// Run `fmt` once over `args[*used..]`, appending to `out` and advancing `used`.
fn format_once(fmt: &str, args: &[String], used: &mut usize, out: &mut String) {
    let cs: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut next_arg = |used: &mut usize| -> Option<&String> {
        let a = args.get(*used);
        if a.is_some() {
            *used += 1;
        }
        a
    };
    while i < cs.len() {
        let c = cs[i];
        i += 1;
        if c == '\\' {
            let esc: String = ['\\', cs.get(i).copied().unwrap_or('\\')].iter().collect();
            if i < cs.len() {
                i += 1;
            }
            out.push_str(&unescape(&esc).0);
            continue;
        }
        if c != '%' {
            out.push(c);
            continue;
        }
        if cs.get(i) == Some(&'%') {
            out.push('%');
            i += 1;
            continue;
        }
        // flags
        let (mut left, mut zero, mut plus, mut space) = (false, false, false, false);
        while let Some(&f) = cs.get(i) {
            match f {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ' ' => space = true,
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        while let Some(d) = cs.get(i).and_then(|c| c.to_digit(10)) {
            width = width * 10 + d as usize;
            i += 1;
        }
        let mut precision: Option<usize> = None;
        if cs.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0usize;
            while let Some(d) = cs.get(i).and_then(|c| c.to_digit(10)) {
                p = p * 10 + d as usize;
                i += 1;
            }
            precision = Some(p);
        }
        let Some(&conv) = cs.get(i) else { break };
        i += 1;
        let arg = next_arg(used);
        let text = match conv {
            's' => {
                let s = arg.map(|a| a.as_str()).unwrap_or("");
                match precision {
                    Some(p) => s.chars().take(p).collect(),
                    None => String::from(s),
                }
            }
            'b' => unescape(arg.map(|a| a.as_str()).unwrap_or("")).0,
            'c' => arg.and_then(|a| a.chars().next()).map(String::from).unwrap_or_default(),
            'd' | 'i' => {
                let n = arg.and_then(|a| a.trim().parse::<i64>().ok()).unwrap_or(0);
                let mut s = alloc::format!("{}", n);
                if n >= 0 && plus {
                    s.insert(0, '+');
                } else if n >= 0 && space {
                    s.insert(0, ' ');
                }
                s
            }
            'u' => alloc::format!("{}", arg.and_then(|a| a.trim().parse::<u64>().ok()).unwrap_or(0)),
            'x' => alloc::format!("{:x}", arg.and_then(|a| a.trim().parse::<u64>().ok()).unwrap_or(0)),
            'X' => alloc::format!("{:X}", arg.and_then(|a| a.trim().parse::<u64>().ok()).unwrap_or(0)),
            'o' => alloc::format!("{:o}", arg.and_then(|a| a.trim().parse::<u64>().ok()).unwrap_or(0)),
            _ => {
                out.push('%');
                out.push(conv);
                continue;
            }
        };
        let len = text.chars().count();
        if len >= width {
            out.push_str(&text);
        } else if left {
            out.push_str(&text);
            out.extend(core::iter::repeat(' ').take(width - len));
        } else if zero && matches!(conv, 'd' | 'i' | 'u' | 'x' | 'X' | 'o') {
            let (sign, digits) = match text.strip_prefix(['-', '+']) {
                Some(d) => (&text[..1], d),
                None => ("", text.as_str()),
            };
            out.push_str(sign);
            out.extend(core::iter::repeat('0').take(width - len));
            out.push_str(digits);
        } else {
            out.extend(core::iter::repeat(' ').take(width - len));
            out.push_str(&text);
        }
    }
}
