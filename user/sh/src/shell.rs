//! The shell's state: variables, functions, jobs, and the services expansion needs.

use crate::fsutil;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use nova::*;
use novashell::{Command, Context};

pub struct Var {
    pub value: String,
    pub exported: bool,
}

/// A command (pipeline) running in the background.
pub struct Job {
    pub id: u32,
    pub pgid: u64,
    /// Process ids still running.
    pub pids: Vec<u64>,
    pub text: String,
    /// Exit status of the last stage, once it has finished.
    pub last_status: Option<i32>,
}

/// Where a command's standard streams go: the shell's own descriptors to hand over (or to
/// use directly, for built-in commands).
#[derive(Clone)]
pub struct Io {
    pub fds: [u64; 3],
    /// More descriptors for the child: (number in the child, our descriptor).
    pub extra: Vec<(u64, u64)>,
}

impl Io {
    pub fn inherit() -> Io {
        Io { fds: [STDIN, STDOUT, STDERR], extra: Vec::new() }
    }
}

pub struct Shell {
    pub vars: BTreeMap<String, Var>,
    pub funcs: BTreeMap<String, Command>,
    pub aliases: BTreeMap<String, String>,
    pub params: Vec<String>,
    pub script_name: String,
    pub status: i32,
    pub pid: u64,
    pub last_bg: u64,
    pub jobs: Vec<Job>,
    pub next_job: u32,
    pub interactive: bool,
    /// Set by `exit`: the shell stops once this is Some.
    pub exit: Option<i32>,
    /// Set by `return` inside a function.
    pub returning: Option<i32>,
    pub loop_depth: u32,
    /// Pending `break N` / `continue N` levels.
    pub break_levels: u32,
    pub continue_levels: u32,
    pub func_depth: u32,
    /// The standard output command substitution currently captures into (None: the real one).
    pub capture: Vec<u64>,
    pub subst_counter: u32,
    pub history: Vec<String>,
}

impl Shell {
    pub fn new(args: &Args, interactive: bool) -> Shell {
        let mut vars = BTreeMap::new();
        for kv in args.env() {
            if let Some((k, v)) = kv.split_once('=') {
                vars.insert(String::from(k), Var { value: String::from(v), exported: true });
            }
        }
        let mut sh = Shell {
            vars,
            funcs: BTreeMap::new(),
            aliases: BTreeMap::new(),
            params: Vec::new(),
            script_name: String::from("sh"),
            status: 0,
            pid: getpid(),
            last_bg: 0,
            jobs: Vec::new(),
            next_job: 1,
            interactive,
            exit: None,
            returning: None,
            loop_depth: 0,
            break_levels: 0,
            continue_levels: 0,
            func_depth: 0,
            capture: Vec::new(),
            subst_counter: 0,
            history: Vec::new(),
        };
        if !sh.vars.contains_key("PATH") {
            sh.set_exported("PATH", "/bin");
        }
        if !sh.vars.contains_key("HOME") {
            sh.set_exported("HOME", "/");
        }
        let cwd = fsutil::cwd();
        sh.set_exported("PWD", &cwd);
        sh
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(|v| v.value.as_str())
    }

    pub fn set(&mut self, name: &str, value: &str) {
        match self.vars.get_mut(name) {
            Some(v) => v.value = String::from(value),
            None => {
                self.vars.insert(String::from(name), Var { value: String::from(value), exported: false });
            }
        }
    }

    pub fn set_exported(&mut self, name: &str, value: &str) {
        self.vars.insert(String::from(name), Var { value: String::from(value), exported: true });
    }

    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
    }

    /// The environment block for a child: the exported variables, plus any `extra` overrides
    /// (assignments in front of the command).
    pub fn env_block(&self, extra: &[(String, String)]) -> Vec<u8> {
        let mut block = Vec::new();
        let mut put = |k: &str, v: &str| {
            block.extend_from_slice(k.as_bytes());
            block.push(b'=');
            block.extend_from_slice(v.as_bytes());
            block.push(0);
        };
        for (k, v) in &self.vars {
            if v.exported && !extra.iter().any(|(ek, _)| ek == k) {
                put(k, &v.value);
            }
        }
        for (k, v) in extra {
            put(k, v);
        }
        block
    }

    /// Print an error message prefixed with the shell's name.
    pub fn complain(&self, message: &str) {
        eprintln!("sh: {}", message);
    }

    pub fn home_dir(&self) -> String {
        self.get("HOME").map(String::from).unwrap_or_else(|| String::from("/"))
    }

    pub fn path_dirs(&self) -> Vec<String> {
        self.get("PATH").unwrap_or("/bin").split(':').filter(|d| !d.is_empty()).map(String::from).collect()
    }

    /// The program file for command `name`, searching PATH unless it contains a slash.
    pub fn find_program(&self, name: &str) -> Option<String> {
        if name.contains('/') {
            return if fsutil::is_file(name) { Some(String::from(name)) } else { None };
        }
        for dir in self.path_dirs() {
            let candidate = fsutil::join(&dir, name);
            if fsutil::is_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

impl Context for Shell {
    fn var(&mut self, name: &str) -> Option<String> {
        self.get(name).map(String::from)
    }

    fn set_var(&mut self, name: &str, value: &str) {
        self.set(name, value);
    }

    fn special(&mut self, name: &str) -> Option<Vec<String>> {
        Some(match name {
            "?" => alloc::vec![alloc::format!("{}", self.status)],
            "$" => alloc::vec![alloc::format!("{}", self.pid)],
            "#" => alloc::vec![alloc::format!("{}", self.params.len())],
            "!" => alloc::vec![alloc::format!("{}", self.last_bg)],
            "-" => alloc::vec![String::new()],
            "@" | "*" => self.params.clone(),
            "0" => alloc::vec![self.script_name.clone()],
            digits => {
                let n: usize = digits.parse().ok()?;
                alloc::vec![self.params.get(n.checked_sub(1)?)?.clone()]
            }
        })
    }

    fn command_output(&mut self, command: &str) -> String {
        self.capture_output(command)
    }

    fn list_dir(&mut self, dir: &str) -> Vec<String> {
        fsutil::list_dir(dir)
    }

    fn is_dir(&mut self, path: &str) -> bool {
        fsutil::is_dir(path)
    }

    fn exists(&mut self, path: &str) -> bool {
        fsutil::exists(path)
    }

    fn home(&mut self) -> String {
        self.home_dir()
    }

    fn error(&mut self, message: &str) {
        self.complain(message);
    }
}

impl Shell {
    /// Run `command` with its standard output collected, and return that output. A temporary
    /// file holds it, so a command that prints more than a pipe can buffer cannot hang the shell.
    pub fn capture_output(&mut self, command: &str) -> String {
        self.subst_counter += 1;
        let path = alloc::format!("/tmp/.sh-{}-{}", self.pid, self.subst_counter);
        let fd = open(&fsutil::cstr(&path), O_WRONLY | O_CREAT | O_TRUNC);
        if fd < 0 {
            self.complain("cannot capture command output (no /tmp?)");
            return String::new();
        }
        self.capture.push(fd as u64);
        let saved_status = self.status;
        self.run_text(command);
        self.capture.pop();
        close(fd as u64);
        let out = fsutil::read_file(&path).unwrap_or_default();
        unlink(&fsutil::cstr(&path));
        // `$?` after a substitution is the substituted command's status, as in other shells.
        let _ = saved_status;
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Where standard output currently goes (the capture file during `$(...)`).
    pub fn stdout_fd(&self) -> u64 {
        self.capture.last().copied().unwrap_or(STDOUT)
    }

    pub fn io_now(&self) -> Io {
        let mut io = Io::inherit();
        io.fds[1] = self.stdout_fd();
        io
    }
}
