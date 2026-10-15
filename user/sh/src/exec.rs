//! Running parsed commands: lists, pipelines, redirections, compound commands, functions,
//! programs.

use crate::fsutil;
use crate::shell::{Io, Job, Shell};
use alloc::string::String;
use alloc::vec::Vec;
use nova::*;
use novashell::{expand_single, expand_word, parse, AndOr, Command, Connector, List, ParseError, Pipeline, Redir, RedirOp};

const ENOEXEC: i64 = -8;

/// The process group a pipeline's programs share (0 until the first one starts).
#[derive(Default)]
pub struct Group {
    pub pgid: u64,
}

pub enum Outcome {
    /// Finished at once (a built-in command, a function, a failure to start).
    Done(i32),
    /// A program was started and has to be waited for.
    Started(u64),
}

impl Shell {
    /// Parse and run text (a script or a command line).
    pub fn run_text(&mut self, text: &str) -> i32 {
        match parse(text) {
            Ok(list) => {
                let io = self.io_now();
                self.run_list(&list, &io)
            }
            Err(ParseError::Incomplete) => {
                self.complain("syntax error: unexpected end of input");
                self.status = 2;
                2
            }
            Err(ParseError::Syntax(m)) => {
                self.complain(&alloc::format!("syntax error: {}", m));
                self.status = 2;
                2
            }
        }
    }

    /// Something happened that makes the rest of the current list pointless.
    pub fn should_stop(&self) -> bool {
        self.exit.is_some() || self.returning.is_some() || self.break_levels > 0 || self.continue_levels > 0
    }

    pub fn run_list(&mut self, list: &List, io: &Io) -> i32 {
        let mut status = self.status;
        for item in list {
            if self.should_stop() {
                break;
            }
            status = if item.background {
                self.run_background(&item.and_or, io)
            } else {
                self.run_and_or(&item.and_or, io)
            };
            self.status = status;
        }
        status
    }

    fn run_and_or(&mut self, ao: &AndOr, io: &Io) -> i32 {
        let mut status = self.run_pipeline(&ao.first, io, false);
        self.status = status;
        for (conn, p) in &ao.rest {
            if self.should_stop() {
                break;
            }
            let run = match conn {
                Connector::And => status == 0,
                Connector::Or => status != 0,
            };
            if run {
                status = self.run_pipeline(p, io, false);
                self.status = status;
            }
        }
        status
    }

    fn run_background(&mut self, ao: &AndOr, io: &Io) -> i32 {
        if !ao.rest.is_empty() {
            self.complain("a background list with && or || is not supported; running it in the foreground");
            return self.run_and_or(ao, io);
        }
        self.run_pipeline(&ao.first, io, true)
    }

    fn run_pipeline(&mut self, p: &Pipeline, io: &Io, background: bool) -> i32 {
        let status = if p.commands.len() == 1 && !background {
            let mut group = Group::default();
            match self.run_command(&p.commands[0], io, &mut group) {
                Outcome::Done(s) => s,
                Outcome::Started(pid) => self.wait_foreground(&[pid], group.pgid, None),
            }
        } else {
            self.run_stages(p, io, background)
        };
        if p.negate {
            (status == 0) as i32
        } else {
            status
        }
    }

    /// Several commands joined by pipes (or one command in the background).
    fn run_stages(&mut self, p: &Pipeline, io: &Io, background: bool) -> i32 {
        let n = p.commands.len();
        let mut pipes: Vec<(u64, u64)> = Vec::new();
        for _ in 0..n.saturating_sub(1) {
            match pipe() {
                Ok(ends) => pipes.push(ends),
                Err(e) => {
                    for (r, w) in pipes {
                        close(r);
                        close(w);
                    }
                    self.complain(&alloc::format!("cannot make a pipe: {}", strerror(e)));
                    return 1;
                }
            }
        }
        let mut group = Group::default();
        let mut started: Vec<u64> = Vec::new();
        let mut last_done: Option<i32> = None;
        for i in 0..n {
            let mut stage = io.clone();
            if i > 0 {
                stage.fds[0] = pipes[i - 1].0;
            }
            if i + 1 < n {
                stage.fds[1] = pipes[i].1;
            }
            match self.run_command(&p.commands[i], &stage, &mut group) {
                Outcome::Started(pid) => {
                    started.push(pid);
                    last_done = None;
                }
                Outcome::Done(s) => last_done = Some(s),
            }
            // Our copies of the ends this stage used must go now, or the others never see the end.
            if i > 0 {
                close(pipes[i - 1].0);
            }
            if i + 1 < n {
                close(pipes[i].1);
            }
        }
        if background {
            if started.is_empty() {
                return last_done.unwrap_or(0);
            }
            let id = self.next_job;
            self.next_job += 1;
            self.last_bg = *started.last().unwrap();
            if self.interactive {
                println!("[{}] {}", id, self.last_bg);
            }
            let text = self.current_line();
            self.jobs.push(Job { id, pgid: group.pgid, pids: started, text, last_status: None });
            return 0;
        }
        self.wait_foreground(&started, group.pgid, last_done)
    }

    /// Wait for the programs of a foreground pipeline. `last_done` is the status of the last stage
    /// if it ran inside the shell.
    pub fn wait_foreground(&mut self, pids: &[u64], pgid: u64, last_done: Option<i32>) -> i32 {
        if pgid != 0 && self.interactive {
            setfg(pgid);
        }
        let mut status = last_done.unwrap_or(0);
        for (i, &pid) in pids.iter().enumerate() {
            let s = self.wait_one(pid);
            if i + 1 == pids.len() && last_done.is_none() {
                status = s;
            }
        }
        if pgid != 0 && self.interactive {
            setfg(0);
        }
        status
    }

    /// Wait for one child and turn its fate into an exit status (printing why, if it was killed).
    pub fn wait_one(&mut self, pid: u64) -> i32 {
        let mut raw = 0i64;
        let r = wait(pid, 0, &mut raw);
        if r < 0 {
            return 1;
        }
        self.decode_status(raw, true)
    }

    pub fn decode_status(&mut self, raw: i64, say: bool) -> i32 {
        if raw >= 0 {
            return (raw & 0xFF) as i32;
        }
        if let Some(sig) = signal_of(raw) {
            if say {
                let text = match sig {
                    SIGINT => "",
                    SIGTERM => "Terminated",
                    SIGKILL => "Killed",
                    SIGHUP => "Hangup",
                    SIGQUIT => "Quit",
                    SIGUSR1 => "User defined signal 1",
                    SIGUSR2 => "User defined signal 2",
                    SIGALRM => "Alarm clock",
                    _ => "Signalled",
                };
                // Control-C is not echoed by the console: the shell supplies the line break.
                println!("{}", text);
            }
            return 128 + sig as i32;
        }
        if raw <= -KILLED_BASE && raw > -SIGNALED_BASE {
            let vector = -raw - KILLED_BASE;
            let (text, code) = match vector {
                0 => ("Division by zero", 136),
                6 => ("Illegal instruction", 132),
                13 => ("General protection fault", 139),
                14 => ("Segmentation fault", 139),
                _ => ("Crashed", 139),
            };
            if say {
                println!("{}", text);
            }
            return code;
        }
        1
    }

    // ---- one command ---------------------------------------------------------------------------

    pub fn run_command(&mut self, cmd: &Command, io: &Io, group: &mut Group) -> Outcome {
        match cmd {
            Command::Simple { assigns, words, redirs } => self.run_simple(assigns, words, redirs, io, group),
            other => Outcome::Done(self.run_compound(other, io)),
        }
    }

    /// A command that must finish before we go on (used inside compound commands).
    fn run_now(&mut self, cmd: &Command, io: &Io) -> i32 {
        let mut group = Group::default();
        match self.run_command(cmd, io, &mut group) {
            Outcome::Done(s) => s,
            Outcome::Started(pid) => self.wait_foreground(&[pid], group.pgid, None),
        }
    }

    fn run_compound(&mut self, cmd: &Command, io: &Io) -> i32 {
        match cmd {
            Command::If { arms, otherwise } => {
                for (cond, body) in arms {
                    let status = self.run_list(cond, io);
                    if self.should_stop() {
                        return status;
                    }
                    if status == 0 {
                        return self.run_list(body, io);
                    }
                }
                match otherwise {
                    Some(body) => self.run_list(body, io),
                    None => 0,
                }
            }
            Command::While { cond, body, until } => {
                self.loop_depth += 1;
                let mut status = 0;
                loop {
                    let c = self.run_list(cond, io);
                    if self.exit.is_some() || self.returning.is_some() {
                        break;
                    }
                    if (c == 0) == *until {
                        break;
                    }
                    status = self.run_list(body, io);
                    if self.loop_should_end() {
                        break;
                    }
                }
                self.loop_depth -= 1;
                status
            }
            Command::For { var, items, body } => {
                let values: Vec<String> = match items {
                    Some(words) => words.iter().flat_map(|w| expand_word(self, w)).collect(),
                    None => self.params.clone(),
                };
                self.loop_depth += 1;
                let mut status = 0;
                for value in values {
                    self.set(var, &value);
                    status = self.run_list(body, io);
                    if self.loop_should_end() {
                        break;
                    }
                }
                self.loop_depth -= 1;
                status
            }
            Command::Group(list) => self.run_list(list, io),
            Command::Function { name, body } => {
                self.funcs.insert(name.clone(), (**body).clone());
                0
            }
            Command::Redirected { command, redirs } => {
                let mut inner = io.clone();
                let mut opened = Vec::new();
                if let Err(msg) = self.apply_redirs(redirs, &mut inner, &mut opened) {
                    self.complain(&msg);
                    for fd in opened {
                        close(fd);
                    }
                    return 1;
                }
                let status = self.run_now(command, &inner);
                for fd in opened {
                    close(fd);
                }
                status
            }
            Command::Simple { .. } => self.run_now(cmd, io),
        }
    }

    /// After a loop body: must the loop end (exit, return, break, or a continue aimed outward)?
    fn loop_should_end(&mut self) -> bool {
        if self.exit.is_some() || self.returning.is_some() {
            return true;
        }
        if self.break_levels > 0 {
            self.break_levels -= 1;
            return true;
        }
        if self.continue_levels > 0 {
            self.continue_levels -= 1;
            return self.continue_levels > 0;
        }
        false
    }

    fn run_simple(
        &mut self,
        assigns: &[(String, novashell::Word)],
        words: &[novashell::Word],
        redirs: &[Redir],
        io: &Io,
        group: &mut Group,
    ) -> Outcome {
        let mut argv: Vec<String> = Vec::new();
        for w in words {
            argv.extend(expand_word(self, w));
        }
        let values: Vec<(String, String)> = assigns.iter().map(|(n, w)| (n.clone(), expand_single(self, w))).collect();

        let mut cio = io.clone();
        let mut opened: Vec<u64> = Vec::new();
        if let Err(msg) = self.apply_redirs(redirs, &mut cio, &mut opened) {
            self.complain(&msg);
            for fd in opened {
                close(fd);
            }
            return Outcome::Done(1);
        }

        let outcome = if argv.is_empty() {
            // Only assignments (and perhaps redirections): they stay.
            for (n, v) in &values {
                self.set(n, v);
            }
            Outcome::Done(0)
        } else {
            self.run_words(&argv, &values, &cio, group)
        };
        for fd in opened {
            close(fd);
        }
        outcome
    }

    pub fn run_words(&mut self, argv: &[String], assigns: &[(String, String)], io: &Io, group: &mut Group) -> Outcome {
        let name = argv[0].as_str();
        if let Some(body) = self.funcs.get(name).cloned() {
            return Outcome::Done(self.call_function(&body, argv, assigns, io));
        }
        if crate::builtins::is_builtin(name) {
            return Outcome::Done(self.run_builtin(name, argv, io));
        }
        self.spawn_program(argv, assigns, io, group)
    }

    fn call_function(&mut self, body: &Command, argv: &[String], assigns: &[(String, String)], io: &Io) -> i32 {
        // Assignments in front of a function call last for the call.
        let saved: Vec<(String, Option<String>)> =
            assigns.iter().map(|(n, _)| (n.clone(), self.get(n).map(String::from))).collect();
        for (n, v) in assigns {
            self.set(n, v);
        }
        let saved_params = core::mem::replace(&mut self.params, argv[1..].to_vec());
        self.func_depth += 1;
        let status = self.run_now(body, io);
        self.func_depth -= 1;
        self.params = saved_params;
        let status = self.returning.take().unwrap_or(status);
        for (n, old) in saved {
            match old {
                Some(v) => self.set(&n, &v),
                None => self.unset(&n),
            }
        }
        status
    }

    // ---- redirections ---------------------------------------------------------------------------------

    /// Open the files a command's redirections name and put them into `io`. Descriptors opened
    /// are added to `opened` (the caller closes them when the command is done).
    pub fn apply_redirs(&mut self, redirs: &[Redir], io: &mut Io, opened: &mut Vec<u64>) -> Result<(), String> {
        for r in redirs {
            let target = expand_single(self, &r.target);
            let set = |io: &mut Io, fd: u32, our: u64| {
                if fd <= 2 {
                    io.fds[fd as usize] = our;
                } else {
                    io.extra.retain(|(c, _)| *c != fd as u64);
                    io.extra.push((fd as u64, our));
                }
            };
            match r.op {
                RedirOp::In | RedirOp::Out | RedirOp::Append | RedirOp::OutAndErr => {
                    let flags = match r.op {
                        RedirOp::In => O_RDONLY,
                        RedirOp::Out | RedirOp::OutAndErr => O_WRONLY | O_CREAT | O_TRUNC,
                        _ => O_WRONLY | O_CREAT | O_APPEND,
                    };
                    let fd = open(&fsutil::cstr(&target), flags);
                    if fd < 0 {
                        return Err(alloc::format!("{}: {}", target, strerror(fd)));
                    }
                    opened.push(fd as u64);
                    set(io, r.fd, fd as u64);
                    if r.op == RedirOp::OutAndErr {
                        set(io, 2, fd as u64);
                    }
                }
                RedirOp::DupOut => {
                    let Ok(from) = target.parse::<u32>() else {
                        return Err(alloc::format!("{}: bad file descriptor", target));
                    };
                    let our = if from <= 2 {
                        io.fds[from as usize]
                    } else {
                        match io.extra.iter().find(|(c, _)| *c == from as u64) {
                            Some((_, o)) => *o,
                            None => return Err(alloc::format!("{}: bad file descriptor", from)),
                        }
                    };
                    set(io, r.fd, our);
                }
            }
        }
        Ok(())
    }

    // ---- starting programs -------------------------------------------------------------------------------

    fn spawn_program(&mut self, argv: &[String], assigns: &[(String, String)], io: &Io, group: &mut Group) -> Outcome {
        let name = argv[0].as_str();
        let Some(path) = self.find_program(name) else {
            self.complain(&alloc::format!("{}: command not found", name));
            return Outcome::Done(127);
        };
        let env = self.env_block(assigns);
        let mut fds: Vec<[u64; 2]> = alloc::vec![[0, io.fds[0]], [1, io.fds[1]], [2, io.fds[2]]];
        for &(child, ours) in &io.extra {
            fds.push([child, ours]);
        }
        let pgid = if group.pgid == 0 { 1 } else { group.pgid };

        let mut r = self.start(&path, argv, &fds, &env, pgid);
        if r == ENOEXEC {
            // Not a program: a script, perhaps. "#!interpreter" names its interpreter; any other
            // text is run by this shell.
            if let Some(interp) = self.script_interpreter(&path) {
                let mut with_script: Vec<String> = alloc::vec![interp.clone(), path.clone()];
                with_script.extend_from_slice(&argv[1..]);
                if let Some(ip) = self.find_program(&interp) {
                    r = self.start(&ip, &with_script, &fds, &env, pgid);
                }
            }
        }
        if r < 0 {
            self.complain(&alloc::format!("{}: cannot run: {}", name, strerror(r)));
            return Outcome::Done(126);
        }
        if group.pgid == 0 {
            group.pgid = r as u64;
        }
        Outcome::Started(r as u64)
    }

    fn start(&self, path: &str, argv: &[String], fds: &[[u64; 2]], env: &[u8], pgid: u64) -> i64 {
        let mut blob: Vec<u8> = Vec::new();
        for a in argv {
            blob.extend_from_slice(a.as_bytes());
            blob.push(0);
        }
        spawn_ex(&fsutil::cstr(path), &blob, argv.len(), fds, Some(env), pgid)
    }

    /// The program that should run the file at `path` if it is a script.
    fn script_interpreter(&self, path: &str) -> Option<String> {
        let data = fsutil::read_file(path).ok()?;
        if data.starts_with(b"#!") {
            let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
            let line = core::str::from_utf8(&data[2..line_end]).ok()?;
            let interp = line.split_whitespace().next()?;
            return Some(String::from(interp.rsplit('/').next().unwrap_or(interp)));
        }
        // Plain text without NULs in its first block: a shell script.
        if data.iter().take(512).all(|&b| b != 0 && (b >= 9)) {
            return Some(String::from("sh"));
        }
        None
    }

    // ---- jobs ----------------------------------------------------------------------------------------------------

    /// What to show for a job started from this command line.
    pub fn current_line(&self) -> String {
        self.history.last().cloned().unwrap_or_default().trim_end_matches('&').trim().into()
    }

    /// Collect background jobs that have finished and say so.
    pub fn reap_jobs(&mut self) {
        loop {
            let mut raw = 0i64;
            let pid = wait(ANY_CHILD, WNOHANG, &mut raw);
            if pid <= 0 {
                return;
            }
            let pid = pid as u64;
            let Some(at) = self.jobs.iter().position(|j| j.pids.contains(&pid)) else { continue };
            let last_pid = *self.jobs[at].pids.last().unwrap_or(&0);
            let status = self.decode_status(raw, false);
            self.jobs[at].pids.retain(|&p| p != pid);
            if pid == last_pid || self.jobs[at].last_status.is_none() {
                self.jobs[at].last_status = Some(status);
            }
            if self.jobs[at].pids.is_empty() {
                let job = self.jobs.remove(at);
                if self.interactive {
                    let how = match job.last_status {
                        Some(0) | None => String::from("Done"),
                        Some(s) if s > 128 && s < 160 => String::from("Terminated"),
                        Some(s) => alloc::format!("Exit {}", s),
                    };
                    println!("[{}]+  {}  {}", job.id, how, job.text);
                }
            }
        }
    }
}
