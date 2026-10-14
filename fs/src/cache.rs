//! A write-through LRU block cache that wraps any device.

use crate::device::{new_block, Block, BlockDevice, IoError, BLOCK_SIZE};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;

struct Entry {
    data: Box<Block>,
    last_used: u64,
}

pub struct CachedDevice<D: BlockDevice> {
    inner: D,
    entries: BTreeMap<u64, Entry>,
    capacity: usize,
    clock: u64,
    pub hits: u64,
    pub misses: u64,
}

impl<D: BlockDevice> CachedDevice<D> {
    /// Keep up to `capacity` blocks in memory.
    pub fn new(inner: D, capacity: usize) -> Self {
        assert!(capacity > 0);
        CachedDevice {
            inner,
            entries: BTreeMap::new(),
            capacity,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    pub fn inner(&self) -> &D {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut D {
        &mut self.inner
    }

    pub fn into_inner(self) -> D {
        self.inner
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn make_room(&mut self) {
        while self.entries.len() >= self.capacity {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(&k, _)| k);
            match oldest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }
}

impl<D: BlockDevice> BlockDevice for CachedDevice<D> {
    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), IoError> {
        let now = self.tick();
        if let Some(entry) = self.entries.get_mut(&index) {
            entry.last_used = now;
            buf.copy_from_slice(&*entry.data);
            self.hits += 1;
            return Ok(());
        }
        self.misses += 1;
        self.inner.read_block(index, buf)?;
        self.make_room();
        let mut data = new_block();
        data.copy_from_slice(&buf[..BLOCK_SIZE]);
        self.entries.insert(index, Entry { data, last_used: now });
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), IoError> {
        // Write through: the device is always up to date, so the cache can be
        // dropped at any time without losing data.
        if let Err(e) = self.inner.write_block(index, buf) {
            self.entries.remove(&index); // do not keep what may not be on disk
            return Err(e);
        }
        let now = self.tick();
        if let Some(entry) = self.entries.get_mut(&index) {
            entry.last_used = now;
            entry.data.copy_from_slice(buf);
        } else {
            self.make_room();
            let mut data = new_block();
            data.copy_from_slice(buf);
            self.entries.insert(index, Entry { data, last_used: now });
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.inner.flush()
    }
}
