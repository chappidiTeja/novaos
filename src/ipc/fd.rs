//! Per-process file descriptor table.

use super::pipe::{PipeReader, PipeWriter};
use crate::net::{TcpSocket, UdpSocket};
use crate::vfs::DiskFile;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub const MAX_FDS: usize = 64;

/// What a file descriptor can refer to.
pub enum File {
    /// The terminal: reads take console input, writes go to the serial port.
    Console,
    PipeRead(PipeReader),
    PipeWrite(PipeWriter),
    /// A file or directory on the disk.
    Disk(DiskFile),
    /// A UDP socket. Dropping the last reference frees its port.
    Udp(UdpSocket),
    /// A TCP socket (a listener or one end of a connection). Closing it ends the connection.
    Tcp(TcpSocket),
}

pub struct FdTable {
    files: Vec<Option<Arc<File>>>,
}

impl FdTable {
    /// A table with stdin, stdout and stderr (0, 1, 2) all on the console.
    pub fn with_stdio() -> Self {
        let mut files = alloc::vec![None; MAX_FDS];
        for fd in 0..3 {
            files[fd] = Some(Arc::new(File::Console));
        }
        FdTable { files }
    }

    /// A table with nothing open.
    pub fn empty() -> Self {
        FdTable { files: alloc::vec![None; MAX_FDS] }
    }

    /// Make `fd` refer to the same open file as `file` (like dup): the position
    /// of a disk file, and the open pipe end, are shared.
    pub fn share(&mut self, fd: usize, file: Arc<File>) {
        self.files[fd] = Some(file);
    }

    pub fn get(&self, fd: u64) -> Option<Arc<File>> {
        self.files.get(fd as usize)?.clone()
    }

    /// Put `file` in the lowest free slot.
    pub fn install(&mut self, file: File) -> Option<usize> {
        let fd = self.files.iter().position(|slot| slot.is_none())?;
        self.files[fd] = Some(Arc::new(file));
        Some(fd)
    }

    /// Put `file` at exactly `fd`, closing whatever was there.
    pub fn install_at(&mut self, fd: usize, file: File) {
        self.files[fd] = Some(Arc::new(file));
    }

    /// Remove `fd` and hand the file back, so the caller can drop it after
    /// releasing any lock around the table.
    pub fn take(&mut self, fd: u64) -> Option<Arc<File>> {
        self.files.get_mut(fd as usize)?.take()
    }

    pub fn close_all(&mut self) {
        for slot in self.files.iter_mut() {
            *slot = None;
        }
    }
}
