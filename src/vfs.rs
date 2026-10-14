//! The kernel's view of the disk: finds the virtio block device, mounts the
//! NovaFS volume on it, and serializes access to it.

use crate::ipc::mutex::Mutex;
use crate::sync::SpinLock;
use crate::virtio_blk::{self, VirtioDisk};
use crate::{pci, println};
use alloc::string::String;
use alloc::vec::Vec;
use novafs::{
    BlockDevice, CachedDevice, DirEntry, FileKind, FsError, NovaFs, Stat, BLOCK_SIZE, ROOT_INO,
};

pub type Fs = NovaFs<CachedDevice<VirtioDisk>>;

/// Blocks kept in memory (512 x 4 KiB = 2 MiB).
const CACHE_BLOCKS: usize = 512;

/// One lock for the whole filesystem. It may be held across disk I/O, which
/// sleeps, so it is a blocking mutex, never a spinlock.
static FS: Mutex<Option<Fs>> = Mutex::new(None);

/// Find the disk and mount the root filesystem.
pub fn init() -> Result<(), String> {
    let devices = pci::scan();
    for d in &devices {
        println!(
            "[pci] {:02x}:{:02x}.{} {:04x}:{:04x} {}",
            d.addr.bus,
            d.addr.slot,
            d.addr.func,
            d.vendor,
            d.device,
            pci::class_name(d.class, d.subclass)
        );
    }
    let block_device = match virtio_blk::probe(&devices) {
        None => return Err("no virtio-blk disk found".into()),
        Some(Err(e)) => return Err(alloc::format!("virtio-blk setup failed: {}", e)),
        Some(Ok(dev)) => dev,
    };
    let disk = VirtioDisk(block_device);
    println!(
        "[ok] disk: virtio-blk, {} MiB",
        disk.block_count() * BLOCK_SIZE as u64 / (1024 * 1024)
    );
    let fs = NovaFs::mount(CachedDevice::new(disk, CACHE_BLOCKS))
        .map_err(|e| alloc::format!("cannot mount the root filesystem: {:?}", e))?;
    let stats = fs.statfs();
    if fs.repaired_counters() {
        println!("[fs] the last shutdown was not clean: free counters were recomputed from the bitmaps");
    }
    println!(
        "[ok] mounted NovaFS: {} of {} blocks free, {} of {} inodes free",
        stats.free_blocks, stats.total_blocks, stats.free_inodes, stats.total_inodes
    );
    *FS.lock() = Some(fs);
    Ok(())
}

/// Inodes whose open file was closed but not yet released in the filesystem.
/// Closing a file can happen where blocking is forbidden (for instance while the
/// scheduler frees a dead process), so `Drop` only records it here and the next
/// filesystem call does the real work.
static PENDING_RELEASES: SpinLock<Vec<u32>> = SpinLock::new(Vec::new());

fn drain_pending(fs: &mut Fs) {
    loop {
        let next = PENDING_RELEASES.lock().pop();
        match next {
            Some(ino) => {
                if let Err(e) = fs.release(ino) {
                    println!("[vfs] could not release inode {}: {:?}", ino, e);
                }
            }
            None => break,
        }
    }
}

/// Run `f` with exclusive access to the filesystem. None if nothing is mounted.
pub fn with_fs<R>(f: impl FnOnce(&mut Fs) -> R) -> Option<R> {
    FS.lock().as_mut().map(|fs| {
        drain_pending(fs);
        f(fs)
    })
}

/// Process files closed since the last filesystem call (frees unlinked files).
pub fn flush_pending() {
    let _ = with_fs(|_| ());
}

// ---- errors -------------------------------------------------------------------

pub const ENOENT: i64 = -2;
pub const EIO: i64 = -5;
pub const EBADF: i64 = -9;
pub const ENODEV: i64 = -19;
pub const EEXIST: i64 = -17;
pub const ENOTDIR: i64 = -20;
pub const EISDIR: i64 = -21;
pub const EINVAL: i64 = -22;
pub const EFBIG: i64 = -27;
pub const ENOSPC: i64 = -28;
pub const ERANGE: i64 = -34;
pub const ENAMETOOLONG: i64 = -36;
pub const ENOTEMPTY: i64 = -39;
pub const EUCLEAN: i64 = -117;

/// The error number a system call reports for a filesystem error.
pub fn errno(e: FsError) -> i64 {
    match e {
        FsError::NotFound => ENOENT,
        FsError::Exists => EEXIST,
        FsError::NotDir => ENOTDIR,
        FsError::IsDir => EISDIR,
        FsError::NotEmpty => ENOTEMPTY,
        FsError::NoSpace => ENOSPC,
        FsError::NameTooLong => ENAMETOOLONG,
        FsError::InvalidName | FsError::InvalidArgument => EINVAL,
        FsError::FileTooBig => EFBIG,
        FsError::Io => EIO,
        FsError::Corrupt(_) => EUCLEAN,
    }
}

/// Run a filesystem operation and translate its result for a system call.
fn call<R>(f: impl FnOnce(&mut Fs) -> Result<R, FsError>) -> Result<R, i64> {
    match with_fs(f) {
        Some(Ok(value)) => Ok(value),
        Some(Err(e)) => Err(errno(e)),
        None => Err(ENODEV),
    }
}

// ---- the current directory ------------------------------------------------------

/// A process's working directory: the inode, and its path for `getcwd`.
#[derive(Clone)]
pub struct Cwd {
    pub ino: u32,
    pub path: String,
}

impl Cwd {
    pub fn root() -> Cwd {
        Cwd { ino: ROOT_INO, path: String::from("/") }
    }
}

/// `path` interpreted from `cwd`, with "." and ".." folded away: an absolute path.
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !path.starts_with('/') {
        parts.extend(cwd.split('/').filter(|p| !p.is_empty()));
    }
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

// ---- open files -----------------------------------------------------------------

pub const O_ACCMODE: u64 = 3;
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0x40;
pub const O_EXCL: u64 = 0x80;
pub const O_TRUNC: u64 = 0x200;
pub const O_APPEND: u64 = 0x400;

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

/// An open file or directory. Descriptors that share one (after a process
/// inherits them) share its position, as on Unix.
pub struct DiskFile {
    pub ino: u32,
    pub kind: FileKind,
    readable: bool,
    writable: bool,
    append: bool,
    pos: SpinLock<u64>,
}

impl Drop for DiskFile {
    fn drop(&mut self) {
        PENDING_RELEASES.lock().push(self.ino);
    }
}

/// Open `path` (relative to `cwd`) with Unix-style `flags`.
pub fn open(path: &str, flags: u64, cwd: u32) -> Result<DiskFile, i64> {
    let access = flags & O_ACCMODE;
    if access == 3 {
        return Err(EINVAL);
    }
    call(|fs| {
        let existing = match fs.resolve(path, cwd) {
            Ok(ino) => Some(ino),
            Err(FsError::NotFound) => None,
            Err(e) => return Err(e),
        };
        let ino = match existing {
            Some(ino) => {
                if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                    return Err(FsError::Exists);
                }
                ino
            }
            None => {
                if flags & O_CREAT == 0 {
                    return Err(FsError::NotFound);
                }
                let (parent, name) = fs.resolve_parent(path, cwd)?;
                fs.create(parent, name, FileKind::File)?
            }
        };
        let stat = fs.stat(ino)?;
        let writing = access == O_WRONLY || access == O_RDWR || flags & O_TRUNC != 0;
        if stat.kind == FileKind::Dir && writing {
            return Err(FsError::IsDir);
        }
        if flags & O_TRUNC != 0 && stat.kind == FileKind::File {
            fs.truncate(ino, 0)?;
        }
        fs.retain(ino)?;
        Ok(DiskFile {
            ino,
            kind: stat.kind,
            readable: access != O_WRONLY,
            writable: access == O_WRONLY || access == O_RDWR,
            append: flags & O_APPEND != 0,
            pos: SpinLock::new(0),
        })
    })
}

impl DiskFile {
    pub fn read(&self, out: &mut [u8]) -> Result<usize, i64> {
        if !self.readable {
            return Err(EBADF);
        }
        if self.kind == FileKind::Dir {
            return Err(EISDIR);
        }
        let start = *self.pos.lock();
        let n = call(|fs| fs.read(self.ino, start, out))?;
        *self.pos.lock() += n as u64;
        Ok(n)
    }

    pub fn write(&self, data: &[u8]) -> Result<usize, i64> {
        if !self.writable {
            return Err(EBADF);
        }
        let start = *self.pos.lock();
        let (offset, n) = call(|fs| {
            let offset = if self.append { fs.stat(self.ino)?.size } else { start };
            Ok((offset, fs.write(self.ino, offset, data)?))
        })?;
        *self.pos.lock() = offset + n as u64;
        Ok(n)
    }

    pub fn seek(&self, offset: i64, whence: u64) -> Result<u64, i64> {
        let base = match whence {
            SEEK_SET => 0i64,
            SEEK_CUR => *self.pos.lock() as i64,
            SEEK_END => call(|fs| fs.stat(self.ino))?.size as i64,
            _ => return Err(EINVAL),
        };
        let target = base.checked_add(offset).ok_or(EINVAL)?;
        if target < 0 {
            return Err(EINVAL);
        }
        *self.pos.lock() = target as u64;
        Ok(target as u64)
    }

    pub fn stat(&self) -> Result<Stat, i64> {
        call(|fs| fs.stat(self.ino))
    }

    /// The next directory entry, advancing the position; None at the end.
    pub fn next_entry(&self) -> Result<Option<DirEntry>, i64> {
        if self.kind != FileKind::Dir {
            return Err(ENOTDIR);
        }
        let cookie = *self.pos.lock();
        let found = call(|fs| fs.readdir(self.ino, cookie))?;
        match found {
            Some((entry, next)) => {
                *self.pos.lock() = next;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }
}

pub fn stat_path(path: &str, cwd: u32) -> Result<Stat, i64> {
    call(|fs| {
        let ino = fs.resolve(path, cwd)?;
        fs.stat(ino)
    })
}

pub fn mkdir(path: &str, cwd: u32) -> Result<(), i64> {
    call(|fs| {
        let (parent, name) = fs.resolve_parent(path, cwd)?;
        fs.create(parent, name, FileKind::Dir).map(|_| ())
    })
}

pub fn unlink(path: &str, cwd: u32) -> Result<(), i64> {
    call(|fs| {
        let (parent, name) = fs.resolve_parent(path, cwd)?;
        fs.unlink(parent, name)
    })
}

pub fn rmdir(path: &str, cwd: u32) -> Result<(), i64> {
    call(|fs| {
        let (parent, name) = fs.resolve_parent(path, cwd)?;
        fs.rmdir(parent, name)
    })
}

/// The inode of directory `path`, for `chdir`.
pub fn resolve_dir(path: &str, cwd: u32) -> Result<u32, i64> {
    call(|fs| {
        let ino = fs.resolve(path, cwd)?;
        match fs.stat(ino)?.kind {
            FileKind::Dir => Ok(ino),
            FileKind::File => Err(FsError::NotDir),
        }
    })
}

/// Read a whole file (for loading programs); at most `limit` bytes.
pub fn read_file(path: &str, cwd: u32, limit: u64) -> Result<Vec<u8>, i64> {
    call(|fs| {
        let ino = fs.resolve(path, cwd)?;
        let stat = fs.stat(ino)?;
        if stat.kind != FileKind::File {
            return Err(FsError::IsDir);
        }
        if stat.size > limit {
            return Err(FsError::FileTooBig);
        }
        let mut data = alloc::vec![0u8; stat.size as usize];
        let mut done = 0;
        while done < data.len() {
            let n = fs.read(ino, done as u64, &mut data[done..])?;
            if n == 0 {
                return Err(FsError::Corrupt("file shorter than its size"));
            }
            done += n;
        }
        Ok(data)
    })
}

pub fn is_mounted() -> bool {
    FS.lock().is_some()
}

// ---- boot-time tests ----------------------------------------------------------

/// The deterministic content of /test/pattern.bin, as the disk builder writes it.
fn pattern_byte(offset: u64) -> u8 {
    (offset ^ (offset >> 8) ^ (offset >> 16)) as u8
}

const PATTERN_SIZE: u64 = 1_572_864;

fn read_whole(fs: &mut Fs, ino: u32) -> Result<Vec<u8>, FsError> {
    let size = fs.stat(ino)?.size as usize;
    let mut out = alloc::vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = fs.read(ino, done as u64, &mut out[done..])?;
        if n == 0 {
            return Err(FsError::Corrupt("file shorter than its size"));
        }
        done += n;
    }
    Ok(out)
}

fn scratch_data() -> Vec<u8> {
    (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect()
}

/// What /tmp/scratch.bin holds after the first round of writes.
fn scratch_expected() -> Vec<u8> {
    let mut data = scratch_data();
    data[100..5100].fill(0xC3);
    data
}

/// Exercises the disk driver and filesystem against the image the build made.
/// Panics on any failure.
pub fn self_test() {
    read_and_write_files();
    remount_from_the_raw_device();
    persistence_across_boots();
}

fn read_and_write_files() {
    with_fs(|fs| {
        fs.check().expect("the filesystem is inconsistent at boot");

        // Files the disk builder put there.
        let hello = fs.resolve("/hello.txt", ROOT_INO).expect("/hello.txt missing");
        assert_eq!(read_whole(fs, hello).unwrap(), b"Hello from NovaFS!\n");
        let nested = fs.resolve("/docs/notes/todo.txt", ROOT_INO).expect("nested file missing");
        assert!(read_whole(fs, nested).unwrap().starts_with(b"1."));
        assert_eq!(fs.resolve("/docs/../hello.txt", ROOT_INO).unwrap(), hello);
        assert_eq!(fs.resolve("/nope", ROOT_INO), Err(FsError::NotFound));

        // A large file, read in awkward chunk sizes and compared byte for byte.
        let pattern = fs.resolve("/test/pattern.bin", ROOT_INO).expect("pattern file missing");
        assert_eq!(fs.stat(pattern).unwrap().size, PATTERN_SIZE);
        let mut buf = alloc::vec![0u8; 70_001];
        let mut offset = 0u64;
        while offset < PATTERN_SIZE {
            let n = fs.read(pattern, offset, &mut buf).unwrap();
            assert!(n > 0);
            for (i, &byte) in buf[..n].iter().enumerate() {
                assert_eq!(byte, pattern_byte(offset + i as u64), "pattern differs at {}", offset + i as u64);
            }
            offset += n as u64;
        }
        println!("[ok] disk: read a {} KiB file and verified every byte", PATTERN_SIZE / 1024);

        // The root directory listing.
        let mut listed = Vec::new();
        let mut cookie = 0;
        while let Some((entry, next)) = fs.readdir(ROOT_INO, cookie).unwrap() {
            cookie = next;
            listed.push(String::from_utf8_lossy(entry.name()).into_owned());
        }
        for expected in ["bin", "docs", "etc", "hello.txt", "test", "tmp", "var"] {
            assert!(listed.iter().any(|n| n == expected), "/{} missing from {:?}", expected, listed);
        }

        // Create, write and read back a new file.
        let tmp = fs.resolve("/tmp", ROOT_INO).unwrap();
        let _ = fs.unlink(tmp, "scratch.bin"); // left over from an earlier boot
        let file = fs.create(tmp, "scratch.bin", FileKind::File).unwrap();
        let data = scratch_data();
        assert_eq!(fs.write(file, 0, &data).unwrap(), data.len());
        assert_eq!(read_whole(fs, file).unwrap(), data);
        fs.write(file, 100, &alloc::vec![0xC3u8; 5000]).unwrap();
        assert_eq!(read_whole(fs, file).unwrap(), scratch_expected());
        fs.sync().unwrap();
        println!("[ok] disk: wrote and re-read a {} KiB file", data.len() / 1024);
    })
    .expect("no filesystem mounted");
}

/// Throw away the filesystem and its cache, test the bare device, then mount
/// again: everything must come back from the disk alone.
fn remount_from_the_raw_device() {
    let fs = FS.lock().take().expect("no filesystem mounted");
    let volume_blocks = fs.volume_blocks();
    let mut disk = fs.into_device().into_inner(); // the block cache is dropped here

    // The image has a scratch area past the end of the volume, for driver tests.
    let total = disk.block_count();
    assert!(total >= volume_blocks + 8, "the disk image has no scratch area");
    let mut block = novafs::new_block();
    let mut check = novafs::new_block();
    for i in 0..8u64 {
        for (j, byte) in block.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(j as u8);
        }
        disk.write_block(volume_blocks + i, &block).unwrap();
    }
    for i in 0..8u64 {
        disk.read_block(volume_blocks + i, &mut check).unwrap();
        for (j, &byte) in check.iter().enumerate() {
            assert_eq!(byte, (i as u8).wrapping_mul(37).wrapping_add(j as u8), "scratch block {} differs", i);
        }
    }
    assert!(disk.read_block(total, &mut check).is_err(), "reading past the end must fail");
    assert!(disk.write_block(total + 5, &block).is_err(), "writing past the end must fail");
    println!("[ok] disk: raw block reads and writes verified, out-of-range rejected");

    let mut fs = NovaFs::mount(CachedDevice::new(disk, CACHE_BLOCKS)).expect("remount failed");
    let file = fs.resolve("/tmp/scratch.bin", ROOT_INO).expect("file lost across remount");
    assert_eq!(read_whole(&mut fs, file).unwrap(), scratch_expected());
    fs.check().expect("inconsistent after remount");
    *FS.lock() = Some(fs);
    println!("[ok] disk: remounted from the raw device, data intact");
}

/// A counter and a log kept on the disk. The test script boots the same disk
/// image twice and expects the second boot to see the first one's writes.
fn persistence_across_boots() {
    with_fs(|fs| {
        let tmp = fs.resolve("/tmp", ROOT_INO).unwrap();
        fs.unlink(tmp, "scratch.bin").unwrap();

        let var = fs.resolve("/var", ROOT_INO).unwrap();
        let boots = match fs.lookup(var, "boots") {
            Ok(ino) => ino,
            Err(FsError::NotFound) => fs.create(var, "boots", FileKind::File).unwrap(),
            Err(e) => panic!("cannot open /var/boots: {:?}", e),
        };
        let previous: u64 = core::str::from_utf8(&read_whole(fs, boots).unwrap())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let this_boot = previous + 1;
        fs.truncate(boots, 0).unwrap();
        fs.write(boots, 0, alloc::format!("{}\n", this_boot).as_bytes()).unwrap();

        let log = match fs.lookup(var, "log.txt") {
            Ok(ino) => ino,
            Err(FsError::NotFound) => fs.create(var, "log.txt", FileKind::File).unwrap(),
            Err(e) => panic!("cannot open /var/log.txt: {:?}", e),
        };
        let size = fs.stat(log).unwrap().size;
        let line = alloc::format!("boot {} reached the filesystem test\n", this_boot);
        fs.write(log, size, line.as_bytes()).unwrap();
        fs.sync().unwrap();

        let history = read_whole(fs, log).unwrap();
        println!("[ok] disk: boot #{} (log has {} lines)", this_boot, history.iter().filter(|&&b| b == b'\n').count());
        fs.check().expect("inconsistent at the end of the disk test");
        println!("[ok] disk self-test");
    })
    .expect("no filesystem mounted");
}
