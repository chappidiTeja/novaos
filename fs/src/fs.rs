//! The filesystem proper.
//!
//! Safety rule for this whole file: nothing read from the device is trusted.
//! Every block pointer is range-checked before use, every size is bounded, and
//! inconsistencies return `FsError::Corrupt` instead of panicking or looping.

use crate::device::{new_block, Block, BlockDevice, BLOCK_SIZE};
use crate::error::FsError;
use crate::layout::*;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub ino: u32,
    pub kind: FileKind,
    pub size: u64,
    pub nlink: u16,
    /// Blocks owned, including index blocks.
    pub blocks: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatFs {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsckReport {
    pub files: u32,
    pub dirs: u32,
    pub blocks_in_use: u32,
}

pub struct NovaFs<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    sb_dirty: bool,
    /// Open-reference counts, so an unlinked file lives until its last user closes it.
    open: BTreeMap<u32, u32>,
    block_hint: u32,
    inode_hint: u32,
    /// Mount found the free counters out of step with the bitmaps and fixed them
    /// (the signature of power being cut in the middle of an update).
    repaired: bool,
}

enum Loc {
    Direct(usize),
    Single(usize),
    Double(usize, usize),
}

fn locate(fb: u64) -> Result<Loc, FsError> {
    if fb < DIRECT as u64 {
        return Ok(Loc::Direct(fb as usize));
    }
    let fb = fb - DIRECT as u64;
    if fb < PTRS_PER_BLOCK {
        return Ok(Loc::Single(fb as usize));
    }
    let fb = fb - PTRS_PER_BLOCK;
    if fb < PTRS_PER_BLOCK * PTRS_PER_BLOCK {
        return Ok(Loc::Double((fb / PTRS_PER_BLOCK) as usize, (fb % PTRS_PER_BLOCK) as usize));
    }
    Err(FsError::FileTooBig)
}

fn div_ceil(a: u64, b: u64) -> u64 {
    (a + b - 1) / b
}

impl<D: BlockDevice> NovaFs<D> {
    // ---- creating and opening a volume -------------------------------------

    /// Write a fresh filesystem using the first `fs_blocks` blocks of `dev`
    /// (any blocks after that are left alone), and return it mounted.
    pub fn format(dev: D, fs_blocks: u64) -> Result<Self, FsError> {
        if fs_blocks > dev.block_count() {
            return Err(FsError::InvalidArgument);
        }
        let geo = Geometry::for_size(fs_blocks)?;
        let mut fs = NovaFs {
            dev,
            sb: Superblock {
                geo,
                free_blocks: geo.total_blocks - geo.data_start,
                free_inodes: geo.inode_count - 1,
            },
            sb_dirty: true,
            open: BTreeMap::new(),
            block_hint: geo.data_start,
            inode_hint: 1,
            repaired: false,
        };

        let zero = new_block();
        for b in 0..geo.data_start {
            fs.write_blk(b, &zero)?;
        }
        // Metadata blocks are permanently "in use"; so is the unusable inode 0.
        fs.mark_prefix(geo.block_bitmap_start, geo.data_start as u64)?;
        fs.mark_prefix(geo.inode_bitmap_start, 1)?;

        let root = fs.alloc_inode()?;
        if root != ROOT_INO {
            return Err(FsError::Corrupt("root inode was not inode 1"));
        }
        let mut inode = Inode::new(KIND_DIR);
        fs.write_inode(root, &inode)?;
        fs.dir_add(root, &mut inode, b".", root, FileKind::Dir)?;
        fs.dir_add(root, &mut inode, b"..", root, FileKind::Dir)?;
        fs.commit()?;
        fs.dev.flush()?;
        Ok(fs)
    }

    /// Open an existing volume, rejecting anything inconsistent.
    pub fn mount(mut dev: D) -> Result<Self, FsError> {
        let mut raw = new_block();
        dev.read_block(0, &mut raw)?;
        let sb = Superblock::decode(&raw)?;
        if sb.geo.total_blocks as u64 > dev.block_count() {
            return Err(FsError::Corrupt("volume is larger than its device"));
        }
        let mut fs = NovaFs {
            dev,
            sb,
            sb_dirty: false,
            open: BTreeMap::new(),
            block_hint: sb.geo.data_start,
            inode_hint: 1,
            repaired: false,
        };
        let root = fs.read_inode(ROOT_INO)?;
        if root.kind != KIND_DIR {
            return Err(FsError::Corrupt("root inode is not a directory"));
        }
        fs.reconcile_counters()?;
        Ok(fs)
    }

    /// Number of set bits among bits [lo, hi) of the bitmap starting at `start`.
    fn count_set(&mut self, start: u32, lo: u64, hi: u64) -> Result<u64, FsError> {
        let mut total = 0u64;
        let mut bit = lo;
        while bit < hi {
            let index = bit / BITS_PER_BLOCK;
            let block_end = ((index + 1) * BITS_PER_BLOCK).min(hi);
            let buf = self.read_blk(start + index as u32)?;
            while bit < block_end {
                let local = (bit % BITS_PER_BLOCK) as usize;
                if local % 8 == 0 && bit + 8 <= block_end {
                    total += buf[local / 8].count_ones() as u64;
                    bit += 8;
                } else {
                    total += (buf[local / 8] >> (local % 8)) as u64 & 1;
                    bit += 1;
                }
            }
        }
        Ok(total)
    }

    /// The free counters live in the superblock, which is written after the
    /// bitmaps; a power cut between the two leaves them wrong. The bitmaps are
    /// the truth, so recompute the counters from them and fix the superblock.
    fn reconcile_counters(&mut self) -> Result<(), FsError> {
        let geo = self.sb.geo;
        let used_blocks =
            self.count_set(geo.block_bitmap_start, geo.data_start as u64, geo.total_blocks as u64)?;
        let used_inodes = self.count_set(geo.inode_bitmap_start, 1, geo.inode_count as u64)?;
        let free_blocks = (geo.total_blocks - geo.data_start) as u64 - used_blocks;
        let free_inodes = (geo.inode_count - 1) as u64 - used_inodes;
        if free_blocks != self.sb.free_blocks as u64 || free_inodes != self.sb.free_inodes as u64 {
            self.sb.free_blocks = free_blocks as u32;
            self.sb.free_inodes = free_inodes as u32;
            self.sb_dirty = true;
            self.repaired = true;
            self.commit()?;
        }
        Ok(())
    }

    /// True if mounting had to correct the free counters, which means the volume
    /// was not shut down cleanly.
    pub fn repaired_counters(&self) -> bool {
        self.repaired
    }

    pub fn into_device(self) -> D {
        self.dev
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.dev
    }

    /// Size of the volume in blocks (the device may be larger).
    pub fn volume_blocks(&self) -> u64 {
        self.sb.geo.total_blocks as u64
    }

    pub fn statfs(&self) -> StatFs {
        StatFs {
            total_blocks: (self.sb.geo.total_blocks - self.sb.geo.data_start) as u64,
            free_blocks: self.sb.free_blocks as u64,
            total_inodes: (self.sb.geo.inode_count - 1) as u64,
            free_inodes: self.sb.free_inodes as u64,
        }
    }

    pub fn sync(&mut self) -> Result<(), FsError> {
        self.commit()?;
        self.dev.flush()?;
        Ok(())
    }

    // ---- raw block access --------------------------------------------------

    fn read_blk(&mut self, block: u32) -> Result<Box<Block>, FsError> {
        let mut buf = new_block();
        self.dev.read_block(block as u64, &mut buf)?;
        Ok(buf)
    }

    fn write_blk(&mut self, block: u32, data: &Block) -> Result<(), FsError> {
        self.dev.write_block(block as u64, data)?;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), FsError> {
        if self.sb_dirty {
            let mut raw = new_block();
            self.sb.encode(&mut raw);
            self.write_blk(0, &raw)?;
            self.sb_dirty = false;
        }
        Ok(())
    }

    /// Run a public operation's result through `commit`, so counters reach the
    /// disk whether or not the operation succeeded.
    fn finish<T>(&mut self, result: Result<T, FsError>) -> Result<T, FsError> {
        let committed = self.commit();
        match (result, committed) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// A pointer that must refer to a real data-area block.
    fn check_ptr(&self, ptr: u32) -> Result<(), FsError> {
        if ptr < self.sb.geo.data_start || ptr >= self.sb.geo.total_blocks {
            Err(FsError::Corrupt("block pointer out of range"))
        } else {
            Ok(())
        }
    }

    // ---- bitmaps and allocation -------------------------------------------

    /// Set the first `count` bits of the bitmap starting at block `start`.
    fn mark_prefix(&mut self, start: u32, count: u64) -> Result<(), FsError> {
        let mut remaining = count;
        let mut block = start;
        while remaining > 0 {
            let mut buf = self.read_blk(block)?;
            let bits = remaining.min(BITS_PER_BLOCK);
            for bit in 0..bits as usize {
                buf[bit / 8] |= 1 << (bit % 8);
            }
            self.write_blk(block, &buf)?;
            remaining -= bits;
            block += 1;
        }
        Ok(())
    }

    /// Set a bit; returns what it was before.
    fn bit_set(&mut self, start: u32, bit: u64, value: bool) -> Result<bool, FsError> {
        let block = start + (bit / BITS_PER_BLOCK) as u32;
        let local = (bit % BITS_PER_BLOCK) as usize;
        let mut buf = self.read_blk(block)?;
        let before = buf[local / 8] & (1 << (local % 8)) != 0;
        if value {
            buf[local / 8] |= 1 << (local % 8);
        } else {
            buf[local / 8] &= !(1 << (local % 8));
        }
        self.write_blk(block, &buf)?;
        Ok(before)
    }

    fn bit_get(&mut self, start: u32, bit: u64) -> Result<bool, FsError> {
        let block = start + (bit / BITS_PER_BLOCK) as u32;
        let local = (bit % BITS_PER_BLOCK) as usize;
        let buf = self.read_blk(block)?;
        Ok(buf[local / 8] & (1 << (local % 8)) != 0)
    }

    /// First clear bit in [lo, hi) of the bitmap starting at `start`.
    fn scan_clear(&mut self, start: u32, lo: u64, hi: u64) -> Result<Option<u64>, FsError> {
        let mut bit = lo;
        while bit < hi {
            let index = bit / BITS_PER_BLOCK;
            let block_end = ((index + 1) * BITS_PER_BLOCK).min(hi);
            let buf = self.read_blk(start + index as u32)?;
            let mut b = bit;
            while b < block_end {
                let local = (b % BITS_PER_BLOCK) as usize;
                let byte = buf[local / 8];
                if local % 8 == 0 && byte == 0xFF && b + 8 <= block_end {
                    b += 8;
                    continue;
                }
                if byte & (1 << (local % 8)) == 0 {
                    return Ok(Some(b));
                }
                b += 1;
            }
            bit = block_end;
        }
        Ok(None)
    }

    /// Allocate a zero-filled data-area block.
    fn alloc_block(&mut self) -> Result<u32, FsError> {
        if self.sb.free_blocks == 0 {
            return Err(FsError::NoSpace);
        }
        let geo = self.sb.geo;
        let (lo, hi) = (geo.data_start as u64, geo.total_blocks as u64);
        let hint = (self.block_hint as u64).clamp(lo, hi);
        let found = match self.scan_clear(geo.block_bitmap_start, hint, hi)? {
            Some(b) => Some(b),
            None => self.scan_clear(geo.block_bitmap_start, lo, hint)?,
        };
        let block = found.ok_or(FsError::Corrupt("free count disagrees with the block bitmap"))?;
        self.bit_set(geo.block_bitmap_start, block, true)?;
        self.sb.free_blocks -= 1;
        self.sb_dirty = true;
        self.block_hint = block as u32 + 1;
        let zero = new_block();
        self.write_blk(block as u32, &zero)?;
        Ok(block as u32)
    }

    fn free_block(&mut self, block: u32) -> Result<(), FsError> {
        self.check_ptr(block)?;
        let was_used = self.bit_set(self.sb.geo.block_bitmap_start, block as u64, false)?;
        if !was_used {
            return Err(FsError::Corrupt("freeing a block that is already free"));
        }
        self.sb.free_blocks += 1;
        self.sb_dirty = true;
        if block < self.block_hint {
            self.block_hint = block;
        }
        Ok(())
    }

    fn alloc_inode(&mut self) -> Result<u32, FsError> {
        if self.sb.free_inodes == 0 {
            return Err(FsError::NoSpace);
        }
        let geo = self.sb.geo;
        let hint = (self.inode_hint as u64).clamp(1, geo.inode_count as u64);
        let found = match self.scan_clear(geo.inode_bitmap_start, hint, geo.inode_count as u64)? {
            Some(i) => Some(i),
            None => self.scan_clear(geo.inode_bitmap_start, 1, hint)?,
        };
        let ino = found.ok_or(FsError::Corrupt("free count disagrees with the inode bitmap"))?;
        self.bit_set(geo.inode_bitmap_start, ino, true)?;
        self.sb.free_inodes -= 1;
        self.sb_dirty = true;
        self.inode_hint = ino as u32 + 1;
        Ok(ino as u32)
    }

    // ---- inodes ------------------------------------------------------------

    fn inode_location(&self, ino: u32) -> Result<(u32, usize), FsError> {
        if ino == 0 || ino >= self.sb.geo.inode_count {
            return Err(FsError::InvalidArgument);
        }
        Ok((
            self.sb.geo.inode_table_start + ino / INODES_PER_BLOCK,
            (ino % INODES_PER_BLOCK) as usize * INODE_SIZE,
        ))
    }

    fn read_inode(&mut self, ino: u32) -> Result<Inode, FsError> {
        let (block, offset) = self.inode_location(ino)?;
        let buf = self.read_blk(block)?;
        let inode = Inode::decode(&buf[offset..offset + INODE_SIZE]);
        match inode.kind {
            KIND_FILE => {}
            KIND_DIR => {
                if inode.size % DIRENT_SIZE as u64 != 0 {
                    return Err(FsError::Corrupt("directory size is not a whole number of entries"));
                }
            }
            KIND_FREE => return Err(FsError::NotFound),
            _ => return Err(FsError::Corrupt("inode has an invalid kind")),
        }
        if inode.size > MAX_FILE_SIZE {
            return Err(FsError::Corrupt("inode size exceeds the maximum file size"));
        }
        Ok(inode)
    }

    fn write_inode(&mut self, ino: u32, inode: &Inode) -> Result<(), FsError> {
        let (block, offset) = self.inode_location(ino)?;
        let mut buf = self.read_blk(block)?;
        inode.encode(&mut buf[offset..offset + INODE_SIZE]);
        self.write_blk(block, &buf)
    }

    pub fn stat(&mut self, ino: u32) -> Result<Stat, FsError> {
        let inode = self.read_inode(ino)?;
        Ok(Stat {
            ino,
            kind: FileKind::from_raw(inode.kind).ok_or(FsError::Corrupt("bad inode kind"))?,
            size: inode.size,
            nlink: inode.nlink,
            blocks: inode.blocks,
        })
    }

    // ---- mapping file blocks to disk blocks -------------------------------

    /// Validate an existing pointer, or allocate one if it is empty and `alloc`.
    fn resolve_ptr(&mut self, ptr: u32, alloc: bool, owned: &mut u32) -> Result<Option<u32>, FsError> {
        if ptr != 0 {
            self.check_ptr(ptr)?;
            return Ok(Some(ptr));
        }
        if !alloc {
            return Ok(None);
        }
        let block = self.alloc_block()?;
        *owned += 1;
        Ok(Some(block))
    }

    /// Entry `slot` of index block `index_block`, allocating it if asked.
    fn index_get(
        &mut self,
        index_block: u32,
        slot: usize,
        alloc: bool,
        owned: &mut u32,
    ) -> Result<Option<u32>, FsError> {
        let mut buf = self.read_blk(index_block)?;
        let current = get_u32(&buf[..], slot * 4);
        if current != 0 {
            self.check_ptr(current)?;
            return Ok(Some(current));
        }
        if !alloc {
            return Ok(None);
        }
        let block = self.alloc_block()?;
        *owned += 1;
        put_u32(&mut buf[..], slot * 4, block);
        self.write_blk(index_block, &buf)?;
        Ok(Some(block))
    }

    /// The disk block holding file block `fb`; None for a hole. With `alloc`,
    /// missing blocks (and the index blocks leading to them) are created.
    fn bmap(&mut self, inode: &mut Inode, fb: u64, alloc: bool) -> Result<Option<u32>, FsError> {
        match locate(fb)? {
            Loc::Direct(i) => {
                let ptr = self.resolve_ptr(inode.direct[i], alloc, &mut inode.blocks)?;
                if let Some(b) = ptr {
                    inode.direct[i] = b;
                }
                Ok(ptr)
            }
            Loc::Single(slot) => {
                let ind = self.resolve_ptr(inode.indirect, alloc, &mut inode.blocks)?;
                let ind = match ind {
                    Some(b) => b,
                    None => return Ok(None),
                };
                inode.indirect = ind;
                self.index_get(ind, slot, alloc, &mut inode.blocks)
            }
            Loc::Double(top, slot) => {
                let dbl = self.resolve_ptr(inode.double_indirect, alloc, &mut inode.blocks)?;
                let dbl = match dbl {
                    Some(b) => b,
                    None => return Ok(None),
                };
                inode.double_indirect = dbl;
                let second = match self.index_get(dbl, top, alloc, &mut inode.blocks)? {
                    Some(b) => b,
                    None => return Ok(None),
                };
                self.index_get(second, slot, alloc, &mut inode.blocks)
            }
        }
    }

    /// Unhook file block `fb` and return the disk block it used.
    fn clear_mapping(&mut self, inode: &mut Inode, fb: u64) -> Result<Option<u32>, FsError> {
        fn take(buf: &mut Block, slot: usize) -> u32 {
            let old = get_u32(&buf[..], slot * 4);
            put_u32(&mut buf[..], slot * 4, 0);
            old
        }
        match locate(fb)? {
            Loc::Direct(i) => {
                let old = core::mem::replace(&mut inode.direct[i], 0);
                Ok(if old == 0 { None } else { Some(old) })
            }
            Loc::Single(slot) => {
                if inode.indirect == 0 {
                    return Ok(None);
                }
                self.check_ptr(inode.indirect)?;
                let mut buf = self.read_blk(inode.indirect)?;
                let old = take(&mut buf, slot);
                if old != 0 {
                    self.write_blk(inode.indirect, &buf)?;
                }
                Ok(if old == 0 { None } else { Some(old) })
            }
            Loc::Double(top, slot) => {
                if inode.double_indirect == 0 {
                    return Ok(None);
                }
                self.check_ptr(inode.double_indirect)?;
                let dbl = self.read_blk(inode.double_indirect)?;
                let second = get_u32(&dbl[..], top * 4);
                if second == 0 {
                    return Ok(None);
                }
                self.check_ptr(second)?;
                let mut buf = self.read_blk(second)?;
                let old = take(&mut buf, slot);
                if old != 0 {
                    self.write_blk(second, &buf)?;
                }
                Ok(if old == 0 { None } else { Some(old) })
            }
        }
    }

    /// One past the highest file block that has any mapping at all. Used when
    /// freeing so that blocks allocated past `size` by an interrupted write are
    /// found too.
    fn mapped_span(&mut self, inode: &Inode) -> Result<u64, FsError> {
        let mut span = 0u64;
        for (i, &ptr) in inode.direct.iter().enumerate() {
            if ptr != 0 {
                span = i as u64 + 1;
            }
        }
        if inode.indirect != 0 {
            self.check_ptr(inode.indirect)?;
            let buf = self.read_blk(inode.indirect)?;
            for slot in 0..PTRS_PER_BLOCK as usize {
                if get_u32(&buf[..], slot * 4) != 0 {
                    span = span.max(DIRECT as u64 + slot as u64 + 1);
                }
            }
        }
        if inode.double_indirect != 0 {
            self.check_ptr(inode.double_indirect)?;
            let dbl = self.read_blk(inode.double_indirect)?;
            for top in 0..PTRS_PER_BLOCK as usize {
                let second = get_u32(&dbl[..], top * 4);
                if second == 0 {
                    continue;
                }
                self.check_ptr(second)?;
                let buf = self.read_blk(second)?;
                for slot in 0..PTRS_PER_BLOCK as usize {
                    if get_u32(&buf[..], slot * 4) != 0 {
                        let fb = DIRECT as u64
                            + PTRS_PER_BLOCK
                            + top as u64 * PTRS_PER_BLOCK
                            + slot as u64;
                        span = span.max(fb + 1);
                    }
                }
            }
        }
        Ok(span)
    }

    fn block_is_empty(&mut self, block: u32) -> Result<bool, FsError> {
        let buf = self.read_blk(block)?;
        Ok(buf.iter().all(|&b| b == 0))
    }

    /// Free index blocks that no longer point at anything.
    fn prune_index_blocks(&mut self, inode: &mut Inode, keep_blocks: u64) -> Result<(), FsError> {
        if inode.double_indirect != 0 {
            self.check_ptr(inode.double_indirect)?;
            let mut dbl = self.read_blk(inode.double_indirect)?;
            // Second-level blocks wholly past the kept range may have emptied.
            let first_top = if keep_blocks > DIRECT as u64 + PTRS_PER_BLOCK {
                ((keep_blocks - DIRECT as u64 - PTRS_PER_BLOCK) / PTRS_PER_BLOCK) as usize
            } else {
                0
            };
            let mut changed = false;
            for top in first_top..PTRS_PER_BLOCK as usize {
                let second = get_u32(&dbl[..], top * 4);
                if second == 0 {
                    continue;
                }
                self.check_ptr(second)?;
                if self.block_is_empty(second)? {
                    self.free_block(second)?;
                    inode.blocks = inode.blocks.checked_sub(1).ok_or(FsError::Corrupt("block count underflow"))?;
                    put_u32(&mut dbl[..], top * 4, 0);
                    changed = true;
                }
            }
            if changed {
                self.write_blk(inode.double_indirect, &dbl)?;
            }
            if dbl.iter().all(|&b| b == 0) {
                self.free_block(inode.double_indirect)?;
                inode.blocks = inode.blocks.checked_sub(1).ok_or(FsError::Corrupt("block count underflow"))?;
                inode.double_indirect = 0;
            }
        }
        if inode.indirect != 0 {
            self.check_ptr(inode.indirect)?;
            if self.block_is_empty(inode.indirect)? {
                self.free_block(inode.indirect)?;
                inode.blocks = inode.blocks.checked_sub(1).ok_or(FsError::Corrupt("block count underflow"))?;
                inode.indirect = 0;
            }
        }
        Ok(())
    }

    /// Cut the file down to `new_size` bytes, freeing what no longer fits.
    fn shrink(&mut self, inode: &mut Inode, new_size: u64) -> Result<(), FsError> {
        let old_blocks = div_ceil(inode.size, BLOCK_SIZE as u64).max(self.mapped_span(inode)?);
        let new_blocks = div_ceil(new_size, BLOCK_SIZE as u64);
        for fb in new_blocks..old_blocks {
            if let Some(block) = self.clear_mapping(inode, fb)? {
                self.free_block(block)?;
                inode.blocks = inode.blocks.checked_sub(1).ok_or(FsError::Corrupt("block count underflow"))?;
            }
        }
        self.prune_index_blocks(inode, new_blocks)?;
        // The part of the last block past the new end must read as zeros if
        // the file grows again.
        if new_size % BLOCK_SIZE as u64 != 0 {
            if let Some(block) = self.bmap(inode, new_size / BLOCK_SIZE as u64, false)? {
                let mut buf = self.read_blk(block)?;
                buf[(new_size % BLOCK_SIZE as u64) as usize..].fill(0);
                self.write_blk(block, &buf)?;
            }
        }
        inode.size = new_size;
        Ok(())
    }

    /// Release an inode and everything it owns.
    fn free_inode_storage(&mut self, ino: u32, inode: &mut Inode) -> Result<(), FsError> {
        self.shrink(inode, 0)?;
        let cleared = Inode { kind: KIND_FREE, nlink: 0, ..Inode::new(KIND_FREE) };
        self.write_inode(ino, &cleared)?;
        let was_used = self.bit_set(self.sb.geo.inode_bitmap_start, ino as u64, false)?;
        if !was_used {
            return Err(FsError::Corrupt("freeing an inode that is already free"));
        }
        self.sb.free_inodes += 1;
        self.sb_dirty = true;
        if ino < self.inode_hint {
            self.inode_hint = ino;
        }
        Ok(())
    }

    // ---- file data ---------------------------------------------------------

    fn read_data(&mut self, inode: &mut Inode, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        if offset >= inode.size {
            return Ok(0);
        }
        let total = (out.len() as u64).min(inode.size - offset) as usize;
        let mut done = 0;
        while done < total {
            let pos = offset + done as u64;
            let within = (pos % BLOCK_SIZE as u64) as usize;
            let chunk = (BLOCK_SIZE - within).min(total - done);
            match self.bmap(inode, pos / BLOCK_SIZE as u64, false)? {
                Some(block) => {
                    let buf = self.read_blk(block)?;
                    out[done..done + chunk].copy_from_slice(&buf[within..within + chunk]);
                }
                None => out[done..done + chunk].fill(0), // a hole
            }
            done += chunk;
        }
        Ok(total)
    }

    /// Write `data` at `offset`, extending the file. Returns bytes written,
    /// which can be short if the disk fills up part way.
    fn write_data(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsError> {
        let end = offset.checked_add(data.len() as u64).ok_or(FsError::FileTooBig)?;
        if end > MAX_FILE_SIZE {
            return Err(FsError::FileTooBig);
        }
        let mut done = 0;
        let mut failure = None;
        while done < data.len() {
            let pos = offset + done as u64;
            let within = (pos % BLOCK_SIZE as u64) as usize;
            let chunk = (BLOCK_SIZE - within).min(data.len() - done);
            let block = match self.bmap(inode, pos / BLOCK_SIZE as u64, true) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    failure = Some(FsError::Corrupt("allocation returned no block"));
                    break;
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            let result = if chunk == BLOCK_SIZE {
                let mut buf = new_block();
                buf.copy_from_slice(&data[done..done + BLOCK_SIZE]);
                self.write_blk(block, &buf)
            } else {
                self.read_blk(block).and_then(|mut buf| {
                    buf[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
                    self.write_blk(block, &buf)
                })
            };
            if let Err(e) = result {
                failure = Some(e);
                break;
            }
            done += chunk;
        }
        if done > 0 {
            inode.size = inode.size.max(offset + done as u64);
        }
        // Even a failed write may have allocated index blocks: always persist.
        self.write_inode(ino, inode)?;
        match failure {
            None => Ok(done),
            Some(FsError::NoSpace) if done > 0 => Ok(done),
            Some(e) => Err(e),
        }
    }

    pub fn read(&mut self, ino: u32, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let mut inode = self.read_inode(ino)?;
        if inode.kind == KIND_DIR {
            return Err(FsError::IsDir);
        }
        self.read_data(&mut inode, offset, out)
    }

    pub fn write(&mut self, ino: u32, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let result = (|| {
            let mut inode = self.read_inode(ino)?;
            if inode.kind == KIND_DIR {
                return Err(FsError::IsDir);
            }
            self.write_data(ino, &mut inode, offset, data)
        })();
        self.finish(result)
    }

    pub fn truncate(&mut self, ino: u32, size: u64) -> Result<(), FsError> {
        let result = (|| {
            let mut inode = self.read_inode(ino)?;
            if inode.kind == KIND_DIR {
                return Err(FsError::IsDir);
            }
            if size > MAX_FILE_SIZE {
                return Err(FsError::FileTooBig);
            }
            if size < inode.size {
                self.shrink(&mut inode, size)?;
            } else {
                inode.size = size; // growing leaves a hole that reads as zeros
            }
            self.write_inode(ino, &inode)
        })();
        self.finish(result)
    }

    // ---- directories -------------------------------------------------------

    /// Visit every slot of a directory from byte offset `start`, free ones as
    /// `None`. Stop early when `visit` returns true.
    fn dir_scan(
        &mut self,
        dir: &mut Inode,
        start: u64,
        mut visit: impl FnMut(u64, Option<DirEntry>) -> bool,
    ) -> Result<(), FsError> {
        let mut offset = start - start % DIRENT_SIZE as u64;
        while offset < dir.size {
            let fb = offset / BLOCK_SIZE as u64;
            let block = self.bmap(dir, fb, false)?;
            let buf = match block {
                Some(b) => Some(self.read_blk(b)?),
                None => None,
            };
            let block_end = ((fb + 1) * BLOCK_SIZE as u64).min(dir.size);
            while offset < block_end {
                let entry = match &buf {
                    Some(buf) => {
                        let at = (offset % BLOCK_SIZE as u64) as usize;
                        DirEntry::decode(&buf[at..at + DIRENT_SIZE])?
                    }
                    None => None,
                };
                if visit(offset, entry) {
                    return Ok(());
                }
                offset += DIRENT_SIZE as u64;
            }
        }
        Ok(())
    }

    fn dir_find(&mut self, dir: &mut Inode, name: &[u8]) -> Result<Option<(DirEntry, u64)>, FsError> {
        let mut found = None;
        self.dir_scan(dir, 0, |offset, entry| match entry {
            Some(e) if e.name() == name => {
                found = Some((e, offset));
                true
            }
            _ => false,
        })?;
        Ok(found)
    }

    /// Add an entry; writes the directory inode back.
    fn dir_add(
        &mut self,
        dir_ino: u32,
        dir: &mut Inode,
        name: &[u8],
        child: u32,
        kind: FileKind,
    ) -> Result<(), FsError> {
        let mut free_slot = None;
        self.dir_scan(dir, 0, |offset, entry| {
            if entry.is_none() {
                free_slot = Some(offset);
                true
            } else {
                false
            }
        })?;
        let offset = free_slot.unwrap_or(dir.size);
        let mut raw = [0u8; DIRENT_SIZE];
        DirEntry::new(child, kind, name).encode(&mut raw);
        let written = self.write_data(dir_ino, dir, offset, &raw)?;
        if written != DIRENT_SIZE {
            return Err(FsError::NoSpace);
        }
        Ok(())
    }

    fn dir_remove_at(&mut self, dir_ino: u32, dir: &mut Inode, offset: u64) -> Result<(), FsError> {
        let raw = [0u8; DIRENT_SIZE];
        let written = self.write_data(dir_ino, dir, offset, &raw)?;
        if written != DIRENT_SIZE {
            return Err(FsError::Io);
        }
        Ok(())
    }

    /// True if the directory holds nothing but "." and "..".
    fn dir_is_empty(&mut self, dir: &mut Inode) -> Result<bool, FsError> {
        let mut empty = true;
        self.dir_scan(dir, 0, |_, entry| match entry {
            Some(e) if e.name() != b"." && e.name() != b".." => {
                empty = false;
                true
            }
            _ => false,
        })?;
        Ok(empty)
    }

    /// Entry named `name` in directory `dir`.
    pub fn lookup(&mut self, dir: u32, name: &str) -> Result<u32, FsError> {
        if name.len() > NAME_MAX {
            return Err(FsError::NameTooLong);
        }
        let mut inode = self.read_inode(dir)?;
        if inode.kind != KIND_DIR {
            return Err(FsError::NotDir);
        }
        match self.dir_find(&mut inode, name.as_bytes())? {
            Some((entry, _)) => Ok(entry.ino),
            None => Err(FsError::NotFound),
        }
    }

    /// The next entry at or after byte offset `cookie` (start with 0), and the
    /// cookie to continue from. None at the end.
    pub fn readdir(&mut self, dir: u32, cookie: u64) -> Result<Option<(DirEntry, u64)>, FsError> {
        let mut inode = self.read_inode(dir)?;
        if inode.kind != KIND_DIR {
            return Err(FsError::NotDir);
        }
        let mut found = None;
        self.dir_scan(&mut inode, cookie, |offset, entry| match entry {
            Some(e) => {
                found = Some((e, offset + DIRENT_SIZE as u64));
                true
            }
            None => false,
        })?;
        Ok(found)
    }

    /// Follow `path` from `cwd` (or from the root if it starts with '/').
    pub fn resolve(&mut self, path: &str, cwd: u32) -> Result<u32, FsError> {
        let mut current = if path.starts_with('/') { ROOT_INO } else { cwd };
        for part in path.split('/').filter(|p| !p.is_empty()) {
            current = self.lookup(current, part)?;
        }
        // A bare relative empty path names the starting directory itself.
        self.stat(current)?;
        Ok(current)
    }

    /// Split `path` into the directory that holds its last component, and that name.
    pub fn resolve_parent<'a>(&mut self, path: &'a str, cwd: u32) -> Result<(u32, &'a str), FsError> {
        let trimmed = path.trim_end_matches('/');
        let (dir_part, name) = match trimmed.rfind('/') {
            Some(i) => (&trimmed[..=i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        if name.is_empty() {
            return Err(FsError::InvalidName);
        }
        let dir = if dir_part.is_empty() {
            if path.starts_with('/') { ROOT_INO } else { cwd }
        } else {
            self.resolve(dir_part, cwd)?
        };
        let kind = self.stat(dir)?.kind;
        if kind != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        Ok((dir, name))
    }

    // ---- creating and removing --------------------------------------------

    pub fn create(&mut self, parent: u32, name: &str, kind: FileKind) -> Result<u32, FsError> {
        let result = self.create_inner(parent, name, kind);
        self.finish(result)
    }

    fn create_inner(&mut self, parent: u32, name: &str, kind: FileKind) -> Result<u32, FsError> {
        validate_new_name(name)?;
        let mut pinode = self.read_inode(parent)?;
        if pinode.kind != KIND_DIR {
            return Err(FsError::NotDir);
        }
        if self.dir_find(&mut pinode, name.as_bytes())?.is_some() {
            return Err(FsError::Exists);
        }
        let ino = self.alloc_inode()?;
        let mut inode = Inode::new(kind.to_raw());
        let built = (|| {
            self.write_inode(ino, &inode)?;
            if kind == FileKind::Dir {
                self.dir_add(ino, &mut inode, b".", ino, FileKind::Dir)?;
                self.dir_add(ino, &mut inode, b"..", parent, FileKind::Dir)?;
            }
            Ok(())
        })();
        // Link into the parent last: until then the new inode is invisible.
        let linked = built.and_then(|()| self.dir_add(parent, &mut pinode, name.as_bytes(), ino, kind));
        if let Err(e) = linked {
            let _ = self.free_inode_storage(ino, &mut inode);
            return Err(e);
        }
        if kind == FileKind::Dir {
            pinode.nlink = pinode.nlink.saturating_add(1);
            self.write_inode(parent, &pinode)?;
        }
        Ok(ino)
    }

    pub fn unlink(&mut self, parent: u32, name: &str) -> Result<(), FsError> {
        let result = self.unlink_inner(parent, name, false);
        self.finish(result)
    }

    pub fn rmdir(&mut self, parent: u32, name: &str) -> Result<(), FsError> {
        let result = self.unlink_inner(parent, name, true);
        self.finish(result)
    }

    fn unlink_inner(&mut self, parent: u32, name: &str, want_dir: bool) -> Result<(), FsError> {
        if name == "." || name == ".." || name.is_empty() {
            return Err(FsError::InvalidName);
        }
        if name.len() > NAME_MAX {
            return Err(FsError::NameTooLong);
        }
        let mut pinode = self.read_inode(parent)?;
        if pinode.kind != KIND_DIR {
            return Err(FsError::NotDir);
        }
        let (entry, offset) = self
            .dir_find(&mut pinode, name.as_bytes())?
            .ok_or(FsError::NotFound)?;
        let mut target = self.read_inode(entry.ino)?;
        match (want_dir, target.kind) {
            (true, KIND_FILE) => return Err(FsError::NotDir),
            (false, KIND_DIR) => return Err(FsError::IsDir),
            _ => {}
        }
        if want_dir && !self.dir_is_empty(&mut target)? {
            return Err(FsError::NotEmpty);
        }

        self.dir_remove_at(parent, &mut pinode, offset)?;
        if want_dir {
            pinode.nlink = pinode.nlink.saturating_sub(1);
            self.write_inode(parent, &pinode)?;
        }
        target.nlink = 0;
        self.write_inode(entry.ino, &target)?;
        // Someone may still have it open: then it is freed when they close it.
        if !self.open.contains_key(&entry.ino) {
            self.free_inode_storage(entry.ino, &mut target)?;
        }
        Ok(())
    }

    // ---- open references ---------------------------------------------------

    /// Note that something holds `ino` open, so unlinking does not free it yet.
    pub fn retain(&mut self, ino: u32) -> Result<(), FsError> {
        self.read_inode(ino)?;
        *self.open.entry(ino).or_insert(0) += 1;
        Ok(())
    }

    /// Drop a reference taken by `retain`; frees the inode if it was unlinked
    /// and this was the last one.
    pub fn release(&mut self, ino: u32) -> Result<(), FsError> {
        let count = self.open.get_mut(&ino).ok_or(FsError::InvalidArgument)?;
        *count -= 1;
        if *count > 0 {
            return Ok(());
        }
        self.open.remove(&ino);
        let result = (|| {
            let mut inode = self.read_inode(ino)?;
            if inode.nlink == 0 {
                self.free_inode_storage(ino, &mut inode)?;
            }
            Ok(())
        })();
        self.finish(result)
    }

    // ---- consistency check -------------------------------------------------

    /// Visit every block an inode owns: data blocks and index blocks.
    fn for_each_block(
        &mut self,
        inode: &Inode,
        mut visit: impl FnMut(u32) -> Result<(), FsError>,
    ) -> Result<(), FsError> {
        for &ptr in inode.direct.iter().filter(|&&p| p != 0) {
            self.check_ptr(ptr)?;
            visit(ptr)?;
        }
        if inode.indirect != 0 {
            self.check_ptr(inode.indirect)?;
            visit(inode.indirect)?;
            let buf = self.read_blk(inode.indirect)?;
            for slot in 0..PTRS_PER_BLOCK as usize {
                let ptr = get_u32(&buf[..], slot * 4);
                if ptr != 0 {
                    self.check_ptr(ptr)?;
                    visit(ptr)?;
                }
            }
        }
        if inode.double_indirect != 0 {
            self.check_ptr(inode.double_indirect)?;
            visit(inode.double_indirect)?;
            let dbl = self.read_blk(inode.double_indirect)?;
            for top in 0..PTRS_PER_BLOCK as usize {
                let second = get_u32(&dbl[..], top * 4);
                if second == 0 {
                    continue;
                }
                self.check_ptr(second)?;
                visit(second)?;
                let buf = self.read_blk(second)?;
                for slot in 0..PTRS_PER_BLOCK as usize {
                    let ptr = get_u32(&buf[..], slot * 4);
                    if ptr != 0 {
                        self.check_ptr(ptr)?;
                        visit(ptr)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Record every block `inode` owns in `used`, failing if any is already
    /// owned, and check the block count the inode keeps.
    fn claim_blocks(&mut self, inode: &Inode, used: &mut BTreeSet<u32>) -> Result<(), FsError> {
        let mut count = 0u32;
        self.for_each_block(inode, |block| {
            if !used.insert(block) {
                return Err(FsError::Corrupt("a block is owned twice"));
            }
            count += 1;
            Ok(())
        })?;
        if count != inode.blocks {
            return Err(FsError::Corrupt("inode block count is wrong"));
        }
        Ok(())
    }

    /// Verify the whole volume: the tree is sound, every block and inode is
    /// owned exactly once, and the bitmaps and counters agree with that.
    pub fn check(&mut self) -> Result<FsckReport, FsError> {
        let geo = self.sb.geo;
        let mut used_blocks: BTreeSet<u32> = BTreeSet::new();
        let mut used_inodes: BTreeSet<u32> = BTreeSet::new();
        let mut files = 0u32;
        let mut dirs = 0u32;

        // (directory inode, its parent)
        let mut pending: Vec<(u32, u32)> = alloc::vec![(ROOT_INO, ROOT_INO)];
        while let Some((dir_ino, parent)) = pending.pop() {
            if !used_inodes.insert(dir_ino) {
                return Err(FsError::Corrupt("a directory is reachable twice"));
            }
            let mut dir = self.read_inode(dir_ino)?;
            if dir.kind != KIND_DIR {
                return Err(FsError::Corrupt("expected a directory"));
            }
            dirs += 1;
            self.claim_blocks(&dir, &mut used_blocks)?;

            let mut entries: Vec<DirEntry> = Vec::new();
            self.dir_scan(&mut dir, 0, |_, entry| {
                if let Some(e) = entry {
                    entries.push(e);
                }
                false
            })?;
            let mut subdirs = 0u16;
            let (mut saw_dot, mut saw_dotdot) = (false, false);
            for entry in entries {
                match entry.name() {
                    b"." => {
                        if entry.ino != dir_ino {
                            return Err(FsError::Corrupt("'.' does not point at its directory"));
                        }
                        saw_dot = true;
                    }
                    b".." => {
                        if entry.ino != parent {
                            return Err(FsError::Corrupt("'..' does not point at the parent"));
                        }
                        saw_dotdot = true;
                    }
                    _ => {
                        let child = self.read_inode(entry.ino)?;
                        if FileKind::from_raw(child.kind) != Some(entry.kind) {
                            return Err(FsError::Corrupt("entry kind disagrees with its inode"));
                        }
                        if child.kind == KIND_DIR {
                            subdirs += 1;
                            pending.push((entry.ino, dir_ino));
                        } else {
                            if !used_inodes.insert(entry.ino) {
                                return Err(FsError::Corrupt("a file is linked twice"));
                            }
                            if child.nlink != 1 {
                                return Err(FsError::Corrupt("file link count is wrong"));
                            }
                            files += 1;
                            self.claim_blocks(&child, &mut used_blocks)?;
                        }
                    }
                }
            }
            if !saw_dot || !saw_dotdot {
                return Err(FsError::Corrupt("directory is missing '.' or '..'"));
            }
            if dir.nlink != 2 + subdirs {
                return Err(FsError::Corrupt("directory link count is wrong"));
            }
        }

        // Unlinked files that are still open are alive too.
        let orphans: Vec<u32> = self.open.keys().copied().collect();
        for ino in orphans {
            let inode = self.read_inode(ino)?;
            if inode.nlink == 0 && used_inodes.insert(ino) {
                self.claim_blocks(&inode, &mut used_blocks)?;
            }
        }

        // The bitmaps must say exactly the same thing.
        for block in geo.data_start..geo.total_blocks {
            let marked = self.bit_get(geo.block_bitmap_start, block as u64)?;
            if marked != used_blocks.contains(&block) {
                return Err(FsError::Corrupt(if marked {
                    "a block is marked used but nothing owns it"
                } else {
                    "a block is in use but marked free"
                }));
            }
        }
        for ino in 1..geo.inode_count {
            let marked = self.bit_get(geo.inode_bitmap_start, ino as u64)?;
            if marked != used_inodes.contains(&ino) {
                return Err(FsError::Corrupt(if marked {
                    "an inode is marked used but unreachable"
                } else {
                    "an inode is in use but marked free"
                }));
            }
        }
        let data_blocks = geo.total_blocks - geo.data_start;
        if self.sb.free_blocks != data_blocks - used_blocks.len() as u32 {
            return Err(FsError::Corrupt("free block counter is wrong"));
        }
        if self.sb.free_inodes != geo.inode_count - 1 - used_inodes.len() as u32 {
            return Err(FsError::Corrupt("free inode counter is wrong"));
        }
        Ok(FsckReport { files, dirs, blocks_in_use: used_blocks.len() as u32 })
    }
}
