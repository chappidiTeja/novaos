//! Byte pipes: a bounded in-kernel buffer with a read end and a write end.
//! Reads sleep while the pipe is empty and a writer exists; writes sleep while
//! it is full and a reader exists. Closing the last writer gives readers EOF;
//! closing the last reader makes writes fail with `BrokenPipe`.

use super::waitq::WaitQueue;
use crate::sync::SpinLock;
use alloc::collections::VecDeque;
use alloc::sync::Arc;

pub const PIPE_CAPACITY: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub struct BrokenPipe;

struct State {
    buf: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

struct Pipe {
    state: SpinLock<State>,
    readable: WaitQueue,
    writable: WaitQueue,
}

pub struct PipeReader(Arc<Pipe>);
pub struct PipeWriter(Arc<Pipe>);

/// Create a pipe with one read end and one write end.
pub fn pipe() -> (PipeReader, PipeWriter) {
    let shared = Arc::new(Pipe {
        state: SpinLock::new(State {
            buf: VecDeque::with_capacity(PIPE_CAPACITY),
            readers: 1,
            writers: 1,
        }),
        readable: WaitQueue::new(),
        writable: WaitQueue::new(),
    });
    (PipeReader(shared.clone()), PipeWriter(shared))
}

impl PipeReader {
    /// Whether `read` would return without sleeping: data is waiting, or the
    /// pipe is empty with no writer left (end of file).
    pub fn ready(&self) -> bool {
        let state = self.0.state.lock();
        !state.buf.is_empty() || state.writers == 0
    }

    /// Read up to `out.len()` bytes, sleeping until at least one is available.
    /// Returns 0 only at end of file (empty and every writer closed).
    pub fn read(&self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let pipe = &*self.0;
        let mut n = 0;
        pipe.readable.wait_until_intr(|| {
            let mut state = pipe.state.lock();
            if state.buf.is_empty() && state.writers > 0 {
                return false;
            }
            while n < out.len() {
                match state.buf.pop_front() {
                    Some(byte) => {
                        out[n] = byte;
                        n += 1;
                    }
                    None => break,
                }
            }
            true
        });
        if n > 0 {
            pipe.writable.wake_all();
        }
        n
    }
}

impl PipeWriter {
    /// Whether a write would make progress without sleeping: there is room, or
    /// no reader is left (the write then fails at once).
    pub fn ready(&self) -> bool {
        let state = self.0.state.lock();
        state.buf.len() < PIPE_CAPACITY || state.readers == 0
    }

    /// Write all of `data`, sleeping while the pipe is full. If every reader is
    /// gone, returns what was written so far, or `BrokenPipe` if nothing was.
    pub fn write(&self, data: &[u8]) -> Result<usize, BrokenPipe> {
        let pipe = &*self.0;
        let mut written = 0;
        while written < data.len() {
            let chunk = &data[written..];
            let mut progressed = 0;
            let mut broken = false;
            let met = pipe.writable.wait_until_intr(|| {
                let mut state = pipe.state.lock();
                if state.readers == 0 {
                    broken = true;
                    return true;
                }
                let space = PIPE_CAPACITY - state.buf.len();
                if space == 0 {
                    return false;
                }
                let n = space.min(chunk.len());
                state.buf.extend(&chunk[..n]);
                progressed = n;
                true
            });
            if !met {
                // The writer was sent a signal that ends it: stop, what we wrote stays written.
                return Ok(written);
            }
            if broken {
                return if written > 0 { Ok(written) } else { Err(BrokenPipe) };
            }
            written += progressed;
            pipe.readable.wake_all();
        }
        Ok(written)
    }
}

impl Clone for PipeReader {
    fn clone(&self) -> Self {
        self.0.state.lock().readers += 1;
        PipeReader(self.0.clone())
    }
}

impl Clone for PipeWriter {
    fn clone(&self) -> Self {
        self.0.state.lock().writers += 1;
        PipeWriter(self.0.clone())
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.0.state.lock().readers -= 1;
        // Blocked writers must notice that nobody is reading any more.
        self.0.writable.wake_all();
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.0.state.lock().writers -= 1;
        // Blocked readers must notice end of file.
        self.0.readable.wake_all();
    }
}
