//! sh: the NovaOS shell.
//!
//!   command args...          run /bin/command (or a path containing '/')
//!   a | b | c                pipeline
//!   cmd > file, >> file      send output to a file (truncate / append)
//!   cmd < file               read input from a file
//!   "quoted words"           quotes group characters; 'single' or "double"
//!   cd [dir], pwd, exit [n], help      built in
//!
//! Line editing: Backspace, Ctrl-C (abandon the line), Ctrl-D (exit on an empty line).

#![no_std]
#![no_main]

use nova::*;

const LINE_MAX: usize = 256;
const MAX_ARGS: usize = 16;
const MAX_STAGES: usize = 6;
const BLOB_MAX: usize = 512;
const FILE_MAX: usize = 128;

/// One command of a pipeline.
struct Stage {
    /// The arguments, NUL-terminated and back to back: exactly what `spawn` takes.
    blob: [u8; BLOB_MAX],
    blob_len: usize,
    starts: [usize; MAX_ARGS],
    argc: usize,
    stdin_file: CString<FILE_MAX>,
    stdout_file: CString<FILE_MAX>,
    has_stdin: bool,
    has_stdout: bool,
    append: bool,
}

impl Stage {
    const EMPTY: Stage = Stage {
        blob: [0; BLOB_MAX],
        blob_len: 0,
        starts: [0; MAX_ARGS],
        argc: 0,
        stdin_file: CString::new(),
        stdout_file: CString::new(),
        has_stdin: false,
        has_stdout: false,
        append: false,
    };

    fn push_byte(&mut self, b: u8) -> bool {
        if self.blob_len + 2 > BLOB_MAX {
            return false;
        }
        self.blob[self.blob_len] = b;
        self.blob_len += 1;
        true
    }

    /// Close off the argument that started at `start`.
    fn finish_arg(&mut self, start: usize) -> bool {
        if self.push_byte(0) {
            self.starts[self.argc] = start;
            self.argc += 1;
            true
        } else {
            false
        }
    }

    fn arg(&self, i: usize) -> &str {
        if i >= self.argc {
            return "";
        }
        let start = self.starts[i];
        let mut end = start;
        while self.blob[end] != 0 {
            end += 1;
        }
        core::str::from_utf8(&self.blob[start..end]).unwrap_or("")
    }
}

/// Read one word starting at `*i`, honouring quotes, handing each character to `push`.
fn read_word(
    line: &[u8],
    i: &mut usize,
    mut push: impl FnMut(u8) -> bool,
) -> Result<(), &'static str> {
    let mut quote: Option<u8> = None;
    while *i < line.len() {
        let c = line[*i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if !push(c) {
                    return Err("word too long");
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b' ' | b'\t' | b'|' | b'>' | b'<' => break,
                _ => {
                    if !push(c) {
                        return Err("word too long");
                    }
                }
            },
        }
        *i += 1;
    }
    if quote.is_some() {
        return Err("unterminated quote");
    }
    Ok(())
}

/// Split a command line into pipeline stages. Returns how many (0 for a blank line).
fn parse(line: &[u8], stages: &mut [Stage; MAX_STAGES]) -> Result<usize, &'static str> {
    let mut i = 0;
    let mut count = 1;
    stages[0] = Stage::EMPTY;
    loop {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        match line[i] {
            b'|' => {
                if stages[count - 1].argc == 0 {
                    return Err("syntax error: nothing before '|'");
                }
                if count == MAX_STAGES {
                    return Err("too many commands in one pipeline");
                }
                stages[count] = Stage::EMPTY;
                count += 1;
                i += 1;
            }
            b'>' => {
                let append = i + 1 < line.len() && line[i + 1] == b'>';
                i += if append { 2 } else { 1 };
                while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
                    i += 1;
                }
                let stage = &mut stages[count - 1];
                let mut file = CString::<FILE_MAX>::new();
                read_word(line, &mut i, |b| file.push_bytes(&[b]))?;
                if file.len() == 0 {
                    return Err("syntax error: missing file name after '>'");
                }
                stage.stdout_file = file;
                stage.has_stdout = true;
                stage.append = append;
            }
            b'<' => {
                i += 1;
                while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
                    i += 1;
                }
                let stage = &mut stages[count - 1];
                let mut file = CString::<FILE_MAX>::new();
                read_word(line, &mut i, |b| file.push_bytes(&[b]))?;
                if file.len() == 0 {
                    return Err("syntax error: missing file name after '<'");
                }
                stage.stdin_file = file;
                stage.has_stdin = true;
            }
            _ => {
                let stage = &mut stages[count - 1];
                if stage.argc == MAX_ARGS {
                    return Err("too many arguments");
                }
                let start = stage.blob_len;
                read_word(line, &mut i, |b| stage.push_byte(b))?;
                if !stage.finish_arg(start) {
                    return Err("command line too long");
                }
            }
        }
    }
    if stages[count - 1].argc == 0 {
        if count > 1 {
            return Err("syntax error: nothing after '|'");
        }
        if stages[0].has_stdin || stages[0].has_stdout {
            return Err("syntax error: redirection without a command");
        }
        return Ok(0);
    }
    Ok(count)
}

fn parse_number(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if negative { -value } else { value })
}

/// Run a built-in command; false if `stage` is not one.
fn builtin(stage: &Stage) -> bool {
    match stage.arg(0) {
        "cd" => {
            let target = if stage.argc > 1 { stage.arg(1) } else { "/" };
            let mut path = CString::<FILE_MAX>::new();
            path.push_str(target);
            let r = chdir(path.as_cstr());
            if r < 0 {
                report("cd", target, r);
            }
            true
        }
        "pwd" => {
            let mut buf = [0u8; 128];
            println!("{}", getcwd(&mut buf).unwrap_or("?"));
            true
        }
        "exit" => {
            let code = if stage.argc > 1 { parse_number(stage.arg(1)).unwrap_or(2) } else { 0 };
            exit(code)
        }
        "help" => {
            println!("Commands: ls cat echo wc cp mkdir rm rmdir touch (programs in /bin)");
            println!("Built in: cd [dir]  pwd  exit [n]  help");
            println!("Use  a | b  to pipe,  > file  >> file  < file  to redirect.");
            true
        }
        _ => false,
    }
}

/// Start every stage of the pipeline, connect them with pipes, and wait.
fn run_pipeline(stages: &[Stage], count: usize) {
    let mut pids = [0u64; MAX_STAGES];
    let mut started = 0;
    // Read end of the previous stage's pipe; we own it until the next stage has it.
    let mut upstream: i64 = -1;

    for (index, stage) in stages[..count].iter().enumerate() {
        let command = stage.arg(0);
        let mut stdio = [STDIN, STDOUT, STDERR];
        let mut own_in: i64 = -1;
        let mut own_out: i64 = -1;
        let mut next_read: i64 = -1;
        let mut ok = true;

        if upstream >= 0 {
            // From here on this stage owns the pipe's read end (it is reassigned below).
            stdio[0] = upstream as u64;
            own_in = upstream;
        }
        if stage.has_stdin {
            let fd = open(stage.stdin_file.as_cstr(), O_RDONLY);
            if fd < 0 {
                report("sh", stage.stdin_file.as_str(), fd);
                ok = false;
            } else {
                if own_in >= 0 {
                    close(own_in as u64);
                }
                stdio[0] = fd as u64;
                own_in = fd;
            }
        }
        if ok && index + 1 < count {
            match pipe() {
                Ok((r, w)) => {
                    stdio[1] = w;
                    own_out = w as i64;
                    next_read = r as i64;
                }
                Err(e) => {
                    report("sh", "pipe", e);
                    ok = false;
                }
            }
        }
        if ok && stage.has_stdout {
            let mode = O_WRONLY | O_CREAT | if stage.append { O_APPEND } else { O_TRUNC };
            let fd = open(stage.stdout_file.as_cstr(), mode);
            if fd < 0 {
                report("sh", stage.stdout_file.as_str(), fd);
                ok = false;
            } else {
                if own_out >= 0 {
                    close(own_out as u64);
                }
                if next_read >= 0 {
                    close(next_read as u64); // nobody will read this pipe now
                    next_read = -1;
                }
                stdio[1] = fd as u64;
                own_out = fd;
            }
        }

        let mut pid: i64 = -1;
        if ok {
            let mut path = CString::<FILE_MAX>::new();
            if !command.contains('/') {
                path.push_str("/bin/");
            }
            path.push_str(command);
            pid = spawn(path.as_cstr(), &stage.blob[..stage.blob_len], stage.argc, Some(&stdio));
            if pid < 0 {
                if pid == -2 {
                    eprintln!("sh: {}: command not found", command);
                } else {
                    report("sh", command, pid);
                }
                ok = false;
            }
        }

        // The new process has its own references now; ours would only keep pipes open.
        if own_in >= 0 {
            close(own_in as u64);
        }
        if own_out >= 0 {
            close(own_out as u64);
        }
        upstream = next_read;
        if !ok {
            break;
        }
        pids[started] = pid as u64;
        started += 1;
    }
    if upstream >= 0 {
        close(upstream as u64);
    }

    let mut last = 0;
    for &pid in &pids[..started] {
        last = waitpid(pid);
    }
    if started == count && last != 0 {
        if last <= -1000 {
            println!("[killed by exception {}]", -last - 1000);
        } else {
            println!("[exit {}]", last);
        }
    }
}

/// Read a line with minimal editing. None at end of input.
fn read_line(buf: &mut [u8; LINE_MAX]) -> Option<usize> {
    let mut len = 0;
    loop {
        let mut byte = [0u8; 1];
        if read(STDIN, &mut byte) <= 0 {
            return None;
        }
        match byte[0] {
            b'\r' | b'\n' => {
                println!();
                return Some(len);
            }
            0x7f | 0x08 => {
                if len > 0 {
                    len -= 1;
                    print!("\x08 \x08");
                }
            }
            0x03 => {
                println!("^C");
                return Some(0);
            }
            0x04 => {
                if len == 0 {
                    return None;
                }
            }
            0x1b => {
                // An escape sequence such as an arrow key: ESC [ letter. Ignore it.
                let mut next = [0u8; 1];
                if read(STDIN, &mut next) == 1 && next[0] == b'[' {
                    read(STDIN, &mut next);
                }
            }
            c @ 0x20..=0x7e => {
                if len < buf.len() {
                    buf[len] = c;
                    len += 1;
                    write(STDOUT, &[c]);
                }
            }
            _ => {}
        }
    }
}

fn main(_args: &Args) -> i32 {
    println!("NovaOS shell. Type 'help' for commands.");
    let mut line = [0u8; LINE_MAX];
    let mut stages = [Stage::EMPTY; MAX_STAGES];
    loop {
        let mut cwd = [0u8; 128];
        print!("nova:{}$ ", getcwd(&mut cwd).unwrap_or("?"));
        let len = match read_line(&mut line) {
            Some(n) => n,
            None => {
                println!("exit");
                return 0;
            }
        };
        match parse(&line[..len], &mut stages) {
            Err(message) => eprintln!("sh: {}", message),
            Ok(0) => {}
            Ok(count) => {
                if count == 1 && builtin(&stages[0]) {
                    continue;
                }
                run_pipeline(&stages, count);
            }
        }
    }
}

entry!(main);
