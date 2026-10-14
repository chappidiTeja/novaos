use alloc::boxed::Box;
use alloc::collections::BTreeMap;

pub const BLOCK_SIZE: usize = 4096;
pub type Block = [u8; BLOCK_SIZE];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoError;

/// Something that stores 4096-byte blocks.
pub trait BlockDevice {
    fn block_count(&self) -> u64;
    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), IoError>;
    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), IoError>;
    /// Make everything written so far durable.
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// A zeroed block on the heap (a 4 KiB array on a small kernel stack adds up).
pub fn new_block() -> Box<Block> {
    alloc::vec![0u8; BLOCK_SIZE]
        .into_boxed_slice()
        .try_into()
        .expect("block has the right length")
}

/// In-memory device. Sparse: blocks never written read as zeros and cost nothing,
/// so tests can use huge sizes.
#[derive(Clone)]
pub struct MemDevice {
    blocks: BTreeMap<u64, Box<Block>>,
    count: u64,
    /// Fail every write after this many more succeed (to test error paths).
    pub fail_writes_after: Option<u64>,
}

impl MemDevice {
    pub fn new(block_count: u64) -> Self {
        MemDevice {
            blocks: BTreeMap::new(),
            count: block_count,
            fail_writes_after: None,
        }
    }

    pub fn raw_mut(&mut self, index: u64) -> &mut Block {
        self.blocks.entry(index).or_insert_with(new_block)
    }

    pub fn touched_blocks(&self) -> usize {
        self.blocks.len()
    }
}

impl BlockDevice for MemDevice {
    fn block_count(&self) -> u64 {
        self.count
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), IoError> {
        if index >= self.count {
            return Err(IoError);
        }
        match self.blocks.get(&index) {
            Some(data) => buf.copy_from_slice(&**data),
            None => buf.fill(0),
        }
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), IoError> {
        if index >= self.count {
            return Err(IoError);
        }
        if let Some(left) = self.fail_writes_after.as_mut() {
            if *left == 0 {
                return Err(IoError);
            }
            *left -= 1;
        }
        self.raw_mut(index).copy_from_slice(buf);
        Ok(())
    }
}
