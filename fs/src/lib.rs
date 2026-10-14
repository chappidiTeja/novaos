//! NovaFS: a small Unix-style filesystem (inodes, bitmaps, direct/indirect
//! blocks, directories as files).
//!
//! `no_std` + `alloc` so the same code runs in the NovaOS kernel and in host
//! tools and tests. It talks to storage only through [`BlockDevice`].

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod cache;
pub mod device;
pub mod error;
mod fs;
pub mod layout;

#[cfg(test)]
mod tests;

pub use cache::CachedDevice;
pub use device::{new_block, Block, BlockDevice, IoError, MemDevice, BLOCK_SIZE};
pub use error::FsError;
pub use fs::{FsckReport, NovaFs, Stat, StatFs};
pub use layout::{DirEntry, FileKind, MAX_FILE_SIZE, NAME_MAX, ROOT_INO};
