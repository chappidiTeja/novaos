//! On-disk format. Everything is little-endian and decoded by hand, so the
//! format does not depend on struct layout or host endianness.
//!
//! Block map (4096-byte blocks):
//!   0                     superblock
//!   inode bitmap          1 bit per inode
//!   block bitmap          1 bit per block of the filesystem
//!   inode table           128-byte inodes
//!   data blocks           file contents, directories, indirect blocks

use crate::device::{Block, BLOCK_SIZE};
use crate::error::FsError;

pub const MAGIC: [u8; 8] = *b"NOVAFS01";
pub const VERSION: u32 = 1;

pub const INODE_SIZE: usize = 128;
pub const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / INODE_SIZE) as u32;
pub const DIRENT_SIZE: usize = 64;
pub const NAME_MAX: usize = 58;
pub const DIRECT: usize = 12;
pub const PTRS_PER_BLOCK: u64 = (BLOCK_SIZE / 4) as u64;
pub const BITS_PER_BLOCK: u64 = (BLOCK_SIZE * 8) as u64;
pub const ROOT_INO: u32 = 1;

/// Largest file: direct + single indirect + double indirect.
pub const MAX_FILE_BLOCKS: u64 = DIRECT as u64 + PTRS_PER_BLOCK + PTRS_PER_BLOCK * PTRS_PER_BLOCK;
pub const MAX_FILE_SIZE: u64 = MAX_FILE_BLOCKS * BLOCK_SIZE as u64;

pub const KIND_FREE: u16 = 0;
pub const KIND_FILE: u16 = 1;
pub const KIND_DIR: u16 = 2;

pub fn get_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn get_u64(b: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(raw)
}

pub fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn div_ceil(a: u64, b: u64) -> u64 {
    (a + b - 1) / b
}

/// Where each region starts, derived from the filesystem size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub total_blocks: u32,
    pub inode_count: u32,
    pub inode_bitmap_start: u32,
    pub inode_bitmap_blocks: u32,
    pub block_bitmap_start: u32,
    pub block_bitmap_blocks: u32,
    pub inode_table_start: u32,
    pub inode_table_blocks: u32,
    pub data_start: u32,
}

impl Geometry {
    /// Layout for a fresh filesystem of `total_blocks` blocks.
    pub fn for_size(total_blocks: u64) -> Result<Geometry, FsError> {
        if total_blocks < 32 || total_blocks > u32::MAX as u64 {
            return Err(FsError::InvalidArgument);
        }
        // About one inode per 16 KiB, in whole inode-table blocks.
        let per = INODES_PER_BLOCK as u64;
        let inodes = (div_ceil(total_blocks / 4, per) * per).max(64);
        Self::from_counts(total_blocks, inodes).ok_or(FsError::InvalidArgument)
    }

    /// The only valid layout for these two numbers, or None if they do not fit.
    pub fn from_counts(total_blocks: u64, inode_count: u64) -> Option<Geometry> {
        if total_blocks < 32 || total_blocks > u32::MAX as u64 {
            return None;
        }
        if inode_count < 2 || inode_count > u32::MAX as u64 {
            return None;
        }
        if inode_count % INODES_PER_BLOCK as u64 != 0 {
            return None;
        }
        let ibm = div_ceil(inode_count, BITS_PER_BLOCK);
        let bbm = div_ceil(total_blocks, BITS_PER_BLOCK);
        let itb = inode_count / INODES_PER_BLOCK as u64;
        let data_start = 1 + ibm + bbm + itb;
        // Leave room for at least a few data blocks.
        if data_start + 8 > total_blocks {
            return None;
        }
        Some(Geometry {
            total_blocks: total_blocks as u32,
            inode_count: inode_count as u32,
            inode_bitmap_start: 1,
            inode_bitmap_blocks: ibm as u32,
            block_bitmap_start: 1 + ibm as u32,
            block_bitmap_blocks: bbm as u32,
            inode_table_start: 1 + (ibm + bbm) as u32,
            inode_table_blocks: itb as u32,
            data_start: data_start as u32,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Superblock {
    pub geo: Geometry,
    pub free_blocks: u32,
    pub free_inodes: u32,
}

/// 32-bit FNV-1a: enough to notice a superblock that is garbage.
fn checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

impl Superblock {
    const BODY_LEN: usize = 8 + 4 * 14;

    pub fn encode(&self, out: &mut Block) {
        out.fill(0);
        out[..8].copy_from_slice(&MAGIC);
        let g = &self.geo;
        let fields = [
            VERSION,
            BLOCK_SIZE as u32,
            g.total_blocks,
            g.inode_count,
            g.inode_bitmap_start,
            g.inode_bitmap_blocks,
            g.block_bitmap_start,
            g.block_bitmap_blocks,
            g.inode_table_start,
            g.inode_table_blocks,
            g.data_start,
            self.free_blocks,
            self.free_inodes,
            ROOT_INO,
        ];
        for (i, value) in fields.iter().enumerate() {
            put_u32(out, 8 + 4 * i, *value);
        }
        let sum = checksum(&out[..Self::BODY_LEN]);
        put_u32(out, Self::BODY_LEN, sum);
    }

    pub fn decode(raw: &Block) -> Result<Superblock, FsError> {
        if raw[..8] != MAGIC {
            return Err(FsError::Corrupt("not a NovaFS volume (bad magic)"));
        }
        if get_u32(raw, Self::BODY_LEN) != checksum(&raw[..Self::BODY_LEN]) {
            return Err(FsError::Corrupt("superblock checksum mismatch"));
        }
        let f = |i: usize| get_u32(raw, 8 + 4 * i);
        if f(0) != VERSION {
            return Err(FsError::Corrupt("unsupported NovaFS version"));
        }
        if f(1) as usize != BLOCK_SIZE {
            return Err(FsError::Corrupt("unsupported block size"));
        }
        if f(13) != ROOT_INO {
            return Err(FsError::Corrupt("unexpected root inode"));
        }
        let geo = Geometry::from_counts(f(2) as u64, f(3) as u64)
            .ok_or(FsError::Corrupt("impossible volume geometry"))?;
        // The stored layout must be exactly the one these sizes imply.
        let stored = [f(4), f(5), f(6), f(7), f(8), f(9), f(10)];
        let derived = [
            geo.inode_bitmap_start,
            geo.inode_bitmap_blocks,
            geo.block_bitmap_start,
            geo.block_bitmap_blocks,
            geo.inode_table_start,
            geo.inode_table_blocks,
            geo.data_start,
        ];
        if stored != derived {
            return Err(FsError::Corrupt("inconsistent volume geometry"));
        }
        let free_blocks = f(11);
        let free_inodes = f(12);
        if free_blocks > geo.total_blocks - geo.data_start || free_inodes >= geo.inode_count {
            return Err(FsError::Corrupt("free counts exceed volume size"));
        }
        Ok(Superblock { geo, free_blocks, free_inodes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode {
    pub kind: u16,
    pub nlink: u16,
    pub size: u64,
    pub direct: [u32; DIRECT],
    pub indirect: u32,
    pub double_indirect: u32,
    /// Blocks owned by this inode: data and index blocks.
    pub blocks: u32,
}

impl Inode {
    pub fn new(kind: u16) -> Inode {
        Inode {
            kind,
            nlink: if kind == KIND_DIR { 2 } else { 1 },
            size: 0,
            direct: [0; DIRECT],
            indirect: 0,
            double_indirect: 0,
            blocks: 0,
        }
    }

    pub fn encode(&self, out: &mut [u8]) {
        out[..INODE_SIZE].fill(0);
        put_u16(out, 0, self.kind);
        put_u16(out, 2, self.nlink);
        put_u64(out, 4, self.size);
        for (i, ptr) in self.direct.iter().enumerate() {
            put_u32(out, 12 + 4 * i, *ptr);
        }
        put_u32(out, 60, self.indirect);
        put_u32(out, 64, self.double_indirect);
        put_u32(out, 68, self.blocks);
    }

    pub fn decode(raw: &[u8]) -> Inode {
        let mut direct = [0u32; DIRECT];
        for (i, ptr) in direct.iter_mut().enumerate() {
            *ptr = get_u32(raw, 12 + 4 * i);
        }
        Inode {
            kind: get_u16(raw, 0),
            nlink: get_u16(raw, 2),
            size: get_u64(raw, 4),
            direct,
            indirect: get_u32(raw, 60),
            double_indirect: get_u32(raw, 64),
            blocks: get_u32(raw, 68),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
}

impl FileKind {
    pub fn to_raw(self) -> u16 {
        match self {
            FileKind::File => KIND_FILE,
            FileKind::Dir => KIND_DIR,
        }
    }

    pub fn from_raw(raw: u16) -> Option<FileKind> {
        match raw {
            KIND_FILE => Some(FileKind::File),
            KIND_DIR => Some(FileKind::Dir),
            _ => None,
        }
    }
}

/// A directory entry: 64 bytes, inode 0 means the slot is free.
#[derive(Clone, Copy, Debug)]
pub struct DirEntry {
    pub ino: u32,
    pub kind: FileKind,
    name: [u8; NAME_MAX],
    name_len: u8,
}

impl DirEntry {
    pub fn new(ino: u32, kind: FileKind, name: &[u8]) -> DirEntry {
        let mut buf = [0u8; NAME_MAX];
        buf[..name.len()].copy_from_slice(name);
        DirEntry { ino, kind, name: buf, name_len: name.len() as u8 }
    }

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    pub fn encode(&self, out: &mut [u8]) {
        out[..DIRENT_SIZE].fill(0);
        put_u32(out, 0, self.ino);
        out[4] = self.kind.to_raw() as u8;
        out[5] = self.name_len;
        out[6..6 + self.name_len as usize].copy_from_slice(self.name());
    }

    /// None for a free slot; Err for an entry that cannot be right.
    pub fn decode(raw: &[u8]) -> Result<Option<DirEntry>, FsError> {
        let ino = get_u32(raw, 0);
        if ino == 0 {
            return Ok(None);
        }
        let kind = FileKind::from_raw(raw[4] as u16)
            .ok_or(FsError::Corrupt("directory entry has an invalid kind"))?;
        let name_len = raw[5] as usize;
        if name_len == 0 || name_len > NAME_MAX {
            return Err(FsError::Corrupt("directory entry has an invalid name length"));
        }
        let mut name = [0u8; NAME_MAX];
        name[..name_len].copy_from_slice(&raw[6..6 + name_len]);
        Ok(Some(DirEntry { ino, kind, name, name_len: name_len as u8 }))
    }
}

/// A name to create: 1..=58 bytes, no '/' or NUL, and not "." or "..".
pub fn validate_new_name(name: &str) -> Result<(), FsError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || name == "." || name == ".." {
        return Err(FsError::InvalidName);
    }
    if bytes.len() > NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    if bytes.iter().any(|&b| b == b'/' || b == 0) {
        return Err(FsError::InvalidName);
    }
    Ok(())
}
