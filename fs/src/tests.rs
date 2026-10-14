use crate::device::{Block, BlockDevice, MemDevice, BLOCK_SIZE};
use crate::layout::*;
use crate::{CachedDevice, FileKind, FsError, NovaFs};
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

fn fresh(blocks: u64) -> NovaFs<MemDevice> {
    NovaFs::format(MemDevice::new(blocks), blocks).expect("format")
}

fn names<D: BlockDevice>(fs: &mut NovaFs<D>, dir: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cookie = 0;
    while let Some((entry, next)) = fs.readdir(dir, cookie).unwrap() {
        cookie = next;
        let name = String::from_utf8_lossy(entry.name()).to_string();
        if name != "." && name != ".." {
            out.push(name);
        }
    }
    out.sort();
    out
}

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn read_all<D: BlockDevice>(fs: &mut NovaFs<D>, ino: u32) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut out = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = fs.read(ino, done as u64, &mut out[done..]).unwrap();
        assert!(n > 0, "short read");
        done += n;
    }
    out
}

#[test]
fn geometry_is_validated() {
    assert!(Geometry::for_size(31).is_err());
    assert!(Geometry::for_size(32).is_ok(), "the smallest volume still fits");
    assert_eq!(Geometry::from_counts(32, 1 << 20), None, "metadata would not fit");
    let g = Geometry::for_size(1024).unwrap();
    assert_eq!(g.inode_table_blocks * INODES_PER_BLOCK, g.inode_count);
    assert!(g.data_start < g.total_blocks);
    assert_eq!(Geometry::from_counts(1024, g.inode_count as u64), Some(g));
    assert_eq!(Geometry::from_counts(1024, 100), None, "inode count must fill whole blocks");
}

#[test]
fn format_then_mount() {
    let mut fs = fresh(1024);
    let report = fs.check().unwrap();
    assert_eq!((report.files, report.dirs), (0, 1));
    assert!(names(&mut fs, ROOT_INO).is_empty());
    let before = fs.statfs();

    let dev = fs.into_device();
    let mut fs = NovaFs::mount(dev).unwrap();
    assert_eq!(fs.statfs(), before);
    fs.check().unwrap();
}

#[test]
fn mount_rejects_garbage() {
    assert!(NovaFs::mount(MemDevice::new(1024)).is_err(), "all-zero disk");

    let fs = fresh(1024);
    let mut dev = fs.into_device();
    let good = dev.clone();

    dev.raw_mut(0)[0] ^= 0xFF; // magic
    assert!(matches!(NovaFs::mount(dev), Err(FsError::Corrupt(_))));

    for byte in [9usize, 16, 20, 40, 60] {
        let mut dev = good.clone();
        dev.raw_mut(0)[byte] ^= 0x01;
        assert!(NovaFs::mount(dev).is_err(), "flipped superblock byte {byte}");
    }

    // A volume that claims to be bigger than the disk under it.
    let mut small = MemDevice::new(512);
    for b in 0..64u64 {
        let src: Block = *good.clone().raw_mut(b);
        *small.raw_mut(b) = src;
    }
    assert!(NovaFs::mount(small).is_err());
}

#[test]
fn file_write_and_read() {
    let mut fs = fresh(1024);
    let ino = fs.create(ROOT_INO, "hello.txt", FileKind::File).unwrap();
    assert_eq!(fs.write(ino, 0, b"hello, world").unwrap(), 12);
    assert_eq!(fs.stat(ino).unwrap().size, 12);

    let mut buf = [0u8; 32];
    assert_eq!(fs.read(ino, 0, &mut buf).unwrap(), 12);
    assert_eq!(&buf[..12], b"hello, world");
    assert_eq!(fs.read(ino, 7, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"world");
    assert_eq!(fs.read(ino, 12, &mut buf).unwrap(), 0, "reading at EOF");
    assert_eq!(fs.read(ino, 99, &mut buf).unwrap(), 0, "reading past EOF");

    // Overwrite in the middle without changing the size.
    fs.write(ino, 7, b"NOVA!").unwrap();
    assert_eq!(read_all(&mut fs, ino), b"hello, NOVA!");
    assert_eq!(fs.stat(ino).unwrap().size, 12);

    assert_eq!(fs.lookup(ROOT_INO, "hello.txt").unwrap(), ino);
    fs.check().unwrap();
}

#[test]
fn large_file_uses_every_mapping_level() {
    // 12 direct blocks (48 KiB) + 1024 single-indirect (4 MiB) + double-indirect.
    let mut fs = fresh(16384);
    let ino = fs.create(ROOT_INO, "big", FileKind::File).unwrap();
    let data = pattern(5 * 1024 * 1024 + 123, 7);
    let mut written = 0;
    while written < data.len() {
        let end = (written + 100_000).min(data.len());
        assert_eq!(fs.write(ino, written as u64, &data[written..end]).unwrap(), end - written);
        written = end;
    }
    let stat = fs.stat(ino).unwrap();
    assert_eq!(stat.size, data.len() as u64);
    assert!(stat.blocks as usize > data.len() / BLOCK_SIZE, "index blocks are counted too");
    assert!(read_all(&mut fs, ino) == data, "large file contents differ");
    fs.check().unwrap();

    let used = fs.statfs().free_blocks;
    fs.unlink(ROOT_INO, "big").unwrap();
    assert!(fs.statfs().free_blocks > used + 1280);
    fs.check().unwrap();
}

#[test]
fn sparse_file_with_distant_write() {
    let mut fs = fresh(262_144); // 1 GiB volume, mostly untouched
    let initial = fs.statfs();
    let ino = fs.create(ROOT_INO, "sparse", FileKind::File).unwrap();
    let far = 2u64 * 1024 * 1024 * 1024; // deep in the double-indirect range
    assert_eq!(fs.write(ino, far, b"end").unwrap(), 3);
    assert_eq!(fs.stat(ino).unwrap().size, far + 3);

    // Only the data block and three index blocks exist.
    let used = initial.free_blocks - fs.statfs().free_blocks;
    assert!(used <= 5, "sparse file used {used} blocks");

    let mut buf = [0xAAu8; 8];
    assert_eq!(fs.read(ino, 0, &mut buf).unwrap(), 8);
    assert_eq!(buf, [0; 8], "holes read as zeros");
    assert_eq!(fs.read(ino, far - 2, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], &[0, 0, b'e', b'n', b'd']);
    fs.check().unwrap();

    fs.truncate(ino, 0).unwrap();
    assert_eq!(
        fs.statfs().free_blocks,
        initial.free_blocks,
        "truncating to zero returns every block"
    );
    fs.check().unwrap();
}

#[test]
fn directories() {
    let mut fs = fresh(2048);
    let a = fs.create(ROOT_INO, "a", FileKind::Dir).unwrap();
    let b = fs.create(a, "b", FileKind::Dir).unwrap();
    let c = fs.create(b, "c", FileKind::Dir).unwrap();
    let f = fs.create(c, "file", FileKind::File).unwrap();

    assert_eq!(fs.resolve("/a/b/c/file", ROOT_INO).unwrap(), f);
    assert_eq!(fs.resolve("a/b", ROOT_INO).unwrap(), b);
    assert_eq!(fs.resolve("/a/b/../b/./c", ROOT_INO).unwrap(), c);
    assert_eq!(fs.resolve("/../../a", ROOT_INO).unwrap(), a, ".. at the root stays at the root");
    assert_eq!(fs.resolve("c/file", b).unwrap(), f, "relative to a directory");
    assert_eq!(fs.resolve("//a///b//", ROOT_INO).unwrap(), b, "repeated slashes");
    assert_eq!(fs.resolve("/a/nope", ROOT_INO), Err(FsError::NotFound));
    assert_eq!(fs.resolve("/a/b/c/file/x", ROOT_INO), Err(FsError::NotDir));

    assert_eq!(fs.resolve_parent("/a/b/new", ROOT_INO).unwrap(), (b, "new"));
    assert_eq!(fs.resolve_parent("new", c).unwrap(), (c, "new"));
    assert_eq!(fs.resolve_parent("/top", ROOT_INO).unwrap(), (ROOT_INO, "top"));
    assert_eq!(fs.resolve_parent("/", ROOT_INO), Err(FsError::InvalidName));

    assert_eq!(fs.rmdir(ROOT_INO, "a"), Err(FsError::NotEmpty));
    assert_eq!(fs.unlink(ROOT_INO, "a"), Err(FsError::IsDir));
    assert_eq!(fs.rmdir(c, "file"), Err(FsError::NotDir));
    assert_eq!(fs.unlink(c, "."), Err(FsError::InvalidName));
    assert_eq!(fs.rmdir(b, ".."), Err(FsError::InvalidName));
    fs.check().unwrap();

    fs.unlink(c, "file").unwrap();
    fs.rmdir(b, "c").unwrap();
    fs.rmdir(a, "b").unwrap();
    fs.rmdir(ROOT_INO, "a").unwrap();
    assert!(names(&mut fs, ROOT_INO).is_empty());
    fs.check().unwrap();
}

#[test]
fn directory_spanning_many_blocks() {
    let mut fs = fresh(4096);
    for i in 0..300 {
        fs.create(ROOT_INO, &format!("file-{i:03}"), FileKind::File).unwrap();
    }
    assert_eq!(names(&mut fs, ROOT_INO).len(), 300);
    // Remove every other one; freed slots must be reused before growing.
    for i in (0..300).step_by(2) {
        fs.unlink(ROOT_INO, &format!("file-{i:03}")).unwrap();
    }
    let size_before = fs.stat(ROOT_INO).unwrap().size;
    for i in 0..150 {
        fs.create(ROOT_INO, &format!("again-{i:03}"), FileKind::File).unwrap();
    }
    assert_eq!(fs.stat(ROOT_INO).unwrap().size, size_before, "free slots are reused");
    assert_eq!(names(&mut fs, ROOT_INO).len(), 300);
    assert!(fs.lookup(ROOT_INO, "file-001").is_ok());
    assert_eq!(fs.lookup(ROOT_INO, "file-000"), Err(FsError::NotFound));
    fs.check().unwrap();
}

#[test]
fn name_rules() {
    let mut fs = fresh(1024);
    let longest = "x".repeat(NAME_MAX);
    fs.create(ROOT_INO, &longest, FileKind::File).unwrap();
    assert_eq!(fs.create(ROOT_INO, &"x".repeat(NAME_MAX + 1), FileKind::File), Err(FsError::NameTooLong));
    for bad in ["", ".", "..", "a/b", "nul\0byte"] {
        assert_eq!(fs.create(ROOT_INO, bad, FileKind::File), Err(FsError::InvalidName), "{bad:?}");
    }
    assert_eq!(fs.create(ROOT_INO, "dup", FileKind::File).map(|_| ()), Ok(()));
    assert_eq!(fs.create(ROOT_INO, "dup", FileKind::File), Err(FsError::Exists));
    assert_eq!(fs.create(ROOT_INO, "dup", FileKind::Dir), Err(FsError::Exists));
    let file = fs.lookup(ROOT_INO, "dup").unwrap();
    assert_eq!(fs.create(file, "child", FileKind::File), Err(FsError::NotDir));
    assert_eq!(fs.create(9999, "x", FileKind::File), Err(FsError::InvalidArgument));
    assert_eq!(fs.lookup(ROOT_INO, &"y".repeat(100)), Err(FsError::NameTooLong));
    fs.check().unwrap();
}

#[test]
fn running_out_of_space() {
    let mut fs = fresh(64);
    let initial = fs.statfs();
    let ino = fs.create(ROOT_INO, "fill", FileKind::File).unwrap();
    let data = vec![0x5Au8; 1 << 20];
    let written = fs.write(ino, 0, &data).unwrap();
    assert!(written > 0 && written < data.len(), "a full disk gives a short write, got {written}");
    assert_eq!(fs.statfs().free_blocks, 0);
    assert_eq!(fs.write(ino, written as u64, &data), Err(FsError::NoSpace));
    assert_eq!(read_all(&mut fs, ino), data[..written]);
    fs.check().unwrap();

    // Directories need blocks too; creating files must fail cleanly, not corrupt.
    assert!(fs.create(ROOT_INO, "more", FileKind::Dir).is_err());
    fs.check().unwrap();

    fs.unlink(ROOT_INO, "fill").unwrap();
    assert_eq!(fs.statfs(), initial);
    fs.check().unwrap();
}

#[test]
fn truncate_shrink_and_regrow() {
    let mut fs = fresh(2048);
    let ino = fs.create(ROOT_INO, "t", FileKind::File).unwrap();
    fs.write(ino, 0, &vec![0xEEu8; 3 * BLOCK_SIZE]).unwrap();

    fs.truncate(ino, 5000).unwrap();
    assert_eq!(fs.stat(ino).unwrap().size, 5000);
    assert_eq!(fs.stat(ino).unwrap().blocks, 2);
    fs.truncate(ino, 20_000).unwrap(); // grow: a hole
    let content = read_all(&mut fs, ino);
    assert!(content[..5000].iter().all(|&b| b == 0xEE));
    assert!(content[5000..].iter().all(|&b| b == 0), "old bytes must not reappear");
    assert_eq!(fs.stat(ino).unwrap().blocks, 2, "growing allocates nothing");

    fs.truncate(ino, 0).unwrap();
    assert_eq!(fs.stat(ino).unwrap().blocks, 0);
    assert_eq!(fs.truncate(ino, MAX_FILE_SIZE + 1), Err(FsError::FileTooBig));
    assert_eq!(fs.write(ino, MAX_FILE_SIZE, b"x"), Err(FsError::FileTooBig));
    assert_eq!(fs.truncate(ROOT_INO, 0), Err(FsError::IsDir));
    fs.check().unwrap();
}

#[test]
fn unlinked_file_lives_until_closed() {
    let mut fs = fresh(1024);
    let initial = fs.statfs();
    let ino = fs.create(ROOT_INO, "temp", FileKind::File).unwrap();
    fs.write(ino, 0, &pattern(20_000, 3)).unwrap();

    fs.retain(ino).unwrap();
    fs.retain(ino).unwrap();
    fs.unlink(ROOT_INO, "temp").unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "temp"), Err(FsError::NotFound));
    assert_eq!(read_all(&mut fs, ino), pattern(20_000, 3), "still readable while open");
    fs.write(ino, 20_000, b"more").unwrap(); // and writable
    fs.check().unwrap(); // an open orphan is not a leak

    fs.release(ino).unwrap();
    assert!(fs.stat(ino).is_ok(), "one reference left");
    fs.release(ino).unwrap();
    assert_eq!(fs.stat(ino), Err(FsError::NotFound), "freed on last close");
    assert_eq!(fs.statfs(), initial);
    assert_eq!(fs.release(ino), Err(FsError::InvalidArgument));
    fs.check().unwrap();
}

#[test]
fn contents_survive_remount() {
    let mut fs = fresh(4096);
    let dir = fs.create(ROOT_INO, "docs", FileKind::Dir).unwrap();
    let a = fs.create(dir, "a.bin", FileKind::File).unwrap();
    let b = fs.create(ROOT_INO, "b.bin", FileKind::File).unwrap();
    let (da, db) = (pattern(70_000, 1), pattern(9_000, 2));
    fs.write(a, 0, &da).unwrap();
    fs.write(b, 0, &db).unwrap();
    let stats = fs.statfs();

    let mut fs = NovaFs::mount(fs.into_device()).unwrap();
    assert_eq!(fs.statfs(), stats);
    let a = fs.resolve("/docs/a.bin", ROOT_INO).unwrap();
    let b = fs.resolve("/b.bin", ROOT_INO).unwrap();
    assert_eq!(read_all(&mut fs, a), da);
    assert_eq!(read_all(&mut fs, b), db);
    fs.check().unwrap();
}

// ---- randomized comparison against a simple in-memory model -----------------

#[derive(Default)]
struct Model {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") }
}

impl Model {
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }
    fn has_children(&self, dir: &str) -> bool {
        let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
        self.files.keys().chain(self.dirs.iter()).any(|p| p != dir && p.starts_with(&prefix))
    }
}

fn run_model_test<D: BlockDevice>(fs: &mut NovaFs<D>, seed: u64, steps: usize) {
    const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
    let mut rng = Rng(seed);
    let mut model = Model::default();
    model.dirs.insert("/".to_string());

    for step in 0..steps {
        let dirs: Vec<String> = model.dirs.iter().cloned().collect();
        let dir = dirs[rng.below(dirs.len() as u64) as usize].clone();
        let name = NAMES[rng.below(NAMES.len() as u64) as usize];
        let path = join(&dir, name);

        match rng.below(7) {
            0 => {
                // mkdir
                let (parent, leaf) = fs.resolve_parent(&path, ROOT_INO).unwrap();
                let result = fs.create(parent, leaf, FileKind::Dir);
                if model.exists(&path) {
                    assert_eq!(result, Err(FsError::Exists), "step {step}: mkdir {path}");
                } else {
                    result.unwrap();
                    model.dirs.insert(path);
                }
            }
            1 | 2 => {
                // write (creating the file if needed)
                if model.dirs.contains(&path) {
                    continue;
                }
                let (parent, leaf) = fs.resolve_parent(&path, ROOT_INO).unwrap();
                let ino = if model.files.contains_key(&path) {
                    fs.lookup(parent, leaf).unwrap()
                } else {
                    model.files.insert(path.clone(), Vec::new());
                    fs.create(parent, leaf, FileKind::File).unwrap()
                };
                let content = model.files.get_mut(&path).unwrap();
                let offset = rng.below(content.len() as u64 + 3000) as usize;
                let data = pattern(rng.below(20_000) as usize, rng.next());
                assert_eq!(fs.write(ino, offset as u64, &data).unwrap(), data.len());
                if content.len() < offset + data.len() {
                    content.resize(offset + data.len(), 0);
                }
                content[offset..offset + data.len()].copy_from_slice(&data);
            }
            3 => {
                // truncate
                if let Some(content) = model.files.get_mut(&path) {
                    let ino = fs.resolve(&path, ROOT_INO).unwrap();
                    let size = rng.below(30_000) as usize;
                    fs.truncate(ino, size as u64).unwrap();
                    content.resize(size, 0);
                }
            }
            4 => {
                // unlink a file
                let (parent, leaf) = fs.resolve_parent(&path, ROOT_INO).unwrap();
                let result = fs.unlink(parent, leaf);
                if model.files.remove(&path).is_some() {
                    result.unwrap();
                } else if model.dirs.contains(&path) {
                    assert_eq!(result, Err(FsError::IsDir));
                } else {
                    assert_eq!(result, Err(FsError::NotFound));
                }
            }
            5 => {
                // rmdir
                let (parent, leaf) = fs.resolve_parent(&path, ROOT_INO).unwrap();
                let result = fs.rmdir(parent, leaf);
                if model.dirs.contains(&path) {
                    if model.has_children(&path) {
                        assert_eq!(result, Err(FsError::NotEmpty), "step {step}: rmdir {path}");
                    } else {
                        result.unwrap();
                        model.dirs.remove(&path);
                    }
                } else if model.files.contains_key(&path) {
                    assert_eq!(result, Err(FsError::NotDir));
                } else {
                    assert_eq!(result, Err(FsError::NotFound));
                }
            }
            _ => {
                // read back a random file and compare
                if let Some(content) = model.files.get(&path) {
                    let ino = fs.resolve(&path, ROOT_INO).unwrap();
                    let offset = rng.below(content.len() as u64 + 100) as usize;
                    let mut buf = vec![0u8; rng.below(10_000) as usize];
                    let n = fs.read(ino, offset as u64, &mut buf).unwrap();
                    let expected = content.get(offset..).unwrap_or(&[]);
                    let expected = &expected[..expected.len().min(buf.len())];
                    assert!(&buf[..n] == expected, "step {step}: read of {path} differs");
                }
            }
        }
        if step % 40 == 0 {
            fs.check().unwrap_or_else(|e| panic!("step {step}: check failed: {e:?}"));
        }
    }

    // Everything must match in full, then everything must go away cleanly.
    for (path, content) in &model.files {
        let ino = fs.resolve(path, ROOT_INO).unwrap();
        assert!(&read_all(fs, ino) == content, "final contents of {path} differ");
    }
    fs.check().unwrap();
    let files: Vec<String> = model.files.keys().cloned().collect();
    for path in files {
        let (parent, leaf) = fs.resolve_parent(&path, ROOT_INO).unwrap();
        fs.unlink(parent, leaf).unwrap();
    }
    let mut dirs: Vec<String> = model.dirs.iter().filter(|d| *d != "/").cloned().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
    for path in dirs {
        let (parent, leaf) = fs.resolve_parent(&path, ROOT_INO).unwrap();
        fs.rmdir(parent, leaf).unwrap();
    }
    assert!(names(fs, ROOT_INO).is_empty());
}

#[test]
fn random_operations_match_the_model() {
    for seed in [1u64, 2, 3, 0xDEAD_BEEF, 0x1234_5678_9ABC] {
        let mut fs = fresh(8192);
        let initial = fs.statfs();
        run_model_test(&mut fs, seed, 600);
        assert_eq!(fs.statfs(), initial, "seed {seed}: leaked blocks or inodes");
        fs.check().unwrap();
    }
}

#[test]
fn random_operations_through_a_tiny_cache() {
    let mut fs = NovaFs::format(CachedDevice::new(MemDevice::new(8192), 8), 8192).unwrap();
    let initial = fs.statfs();
    run_model_test(&mut fs, 99, 500);
    assert_eq!(fs.statfs(), initial);
    fs.check().unwrap();
    let cached = fs.into_device();
    assert!(cached.hits > 0 && cached.misses > 0, "cache saw no traffic");
    // The device underneath must be a complete, valid filesystem on its own.
    let mut bare = NovaFs::mount(cached.into_inner()).unwrap();
    bare.check().unwrap();
}

// ---- hostile and failing devices --------------------------------------------

fn walk<D: BlockDevice>(fs: &mut NovaFs<D>, dir: u32, depth: u32, budget: &mut u32) {
    if depth > 12 {
        return;
    }
    let mut cookie = 0;
    while *budget > 0 {
        *budget -= 1;
        let Ok(Some((entry, next))) = fs.readdir(dir, cookie) else { return };
        assert!(next > cookie, "readdir must make progress");
        cookie = next;
        if entry.name() == b"." || entry.name() == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(entry.name()).to_string();
        let _ = fs.lookup(dir, &name);
        let _ = fs.stat(entry.ino);
        match entry.kind {
            FileKind::Dir => walk(fs, entry.ino, depth + 1, budget),
            FileKind::File => {
                let mut buf = vec![0u8; BLOCK_SIZE];
                let mut offset = 0;
                for _ in 0..64 {
                    match fs.read(entry.ino, offset, &mut buf) {
                        Ok(n) if n > 0 => offset += n as u64,
                        _ => break,
                    }
                }
            }
        }
    }
}

fn populated_image() -> MemDevice {
    let mut fs = fresh(2048);
    for d in 0..4 {
        let dir = fs.create(ROOT_INO, &format!("dir{d}"), FileKind::Dir).unwrap();
        for f in 0..6 {
            let ino = fs.create(dir, &format!("f{f}"), FileKind::File).unwrap();
            let len = [10, 5000, 60_000, 300_000, 0, 4096][f];
            fs.write(ino, 0, &pattern(len, (d * 10 + f) as u64)).unwrap();
        }
    }
    fs.into_device()
}

#[test]
fn corrupted_images_never_crash_the_filesystem() {
    let pristine = populated_image();
    let mut rng = Rng(0xC0FFEE);
    let (mut mounted, mut rejected) = (0, 0);
    for round in 0..3000 {
        let mut dev = pristine.clone();
        // Mostly damage metadata (superblock, bitmaps, inode table, directories).
        let hot = 40u64;
        for _ in 0..1 + rng.below(8) {
            let block = if rng.below(4) == 0 { rng.below(2048) } else { rng.below(hot) };
            // Real structures sit at the start of each block (the superblock
            // uses 68 bytes), so aim there half the time.
            let at = if rng.below(2) == 0 {
                rng.below(BLOCK_SIZE as u64)
            } else {
                rng.below(160)
            } as usize;
            dev.raw_mut(block)[at] = rng.next() as u8;
        }
        match NovaFs::mount(dev) {
            Err(_) => rejected += 1,
            Ok(mut fs) => {
                mounted += 1;
                let mut budget = 3000;
                walk(&mut fs, ROOT_INO, 0, &mut budget);
                // Mutating a damaged volume must also fail cleanly, not crash.
                let _ = fs.create(ROOT_INO, "fuzz", FileKind::File);
                if let Ok(ino) = fs.lookup(ROOT_INO, "fuzz") {
                    let _ = fs.write(ino, 0, &pattern(9000, round));
                    let _ = fs.truncate(ino, 100);
                    let _ = fs.unlink(ROOT_INO, "fuzz");
                }
                let _ = fs.rmdir(ROOT_INO, "dir0");
                let _ = fs.check();
            }
        }
    }
    assert!(mounted > 50 && rejected > 10, "fuzzing should exercise both paths ({mounted}/{rejected})");
}

#[test]
fn damage_is_noticed_by_check() {
    let pristine = populated_image();
    NovaFs::mount(pristine.clone()).unwrap().check().unwrap();

    // Flip one block-bitmap bit for a block that is really in use.
    let mut dev = pristine.clone();
    let fs = NovaFs::mount(pristine.clone()).unwrap();
    let geo = Geometry::for_size(2048).unwrap();
    drop(fs);
    let victim = geo.data_start as usize + 3;
    let map = dev.raw_mut(geo.block_bitmap_start as u64);
    assert!(map[victim / 8] & (1 << (victim % 8)) != 0);
    map[victim / 8] &= !(1 << (victim % 8));
    let mut fs = NovaFs::mount(dev).unwrap();
    assert!(matches!(fs.check(), Err(FsError::Corrupt(_))));

    // A leaked block: marked used, owned by nobody.
    let mut dev = pristine;
    let spare = geo.total_blocks as usize - 1;
    dev.raw_mut(geo.block_bitmap_start as u64)[spare / 8] |= 1 << (spare % 8);
    let mut fs = NovaFs::mount(dev).unwrap();
    assert!(matches!(fs.check(), Err(FsError::Corrupt(_))));
}

#[test]
fn failing_writes_become_errors_not_panics() {
    for allowed in 0..60u64 {
        let mut fs = fresh(1024);
        fs.device_mut().fail_writes_after = Some(allowed);
        let dir = fs.create(ROOT_INO, "d", FileKind::Dir);
        let file = dir.and_then(|d| fs.create(d, "f", FileKind::File));
        if let Ok(ino) = file {
            let _ = fs.write(ino, 0, &pattern(30_000, allowed));
            let _ = fs.truncate(ino, 10);
        }
        let _ = fs.unlink(ROOT_INO, "d");
        let _ = fs.sync();
    }
}

#[test]
fn torn_counters_are_repaired_at_mount() {
    let mut fs = fresh(2048);
    let ino = fs.create(ROOT_INO, "f", FileKind::File).unwrap();
    fs.write(ino, 0, &pattern(50_000, 5)).unwrap();
    let truth = fs.statfs();
    assert!(!fs.repaired_counters());

    // Power cut between writing the bitmaps and writing the superblock: the
    // superblock still holds counters from before the last update.
    for (blocks_off, inodes_off) in [(7i64, 1i64), (-3, -1)] {
        let mut dev = fs.into_device();
        let mut raw: Block = *dev.raw_mut(0);
        let mut sb = Superblock::decode(&raw).unwrap();
        sb.free_blocks = (sb.free_blocks as i64 + blocks_off) as u32;
        sb.free_inodes = (sb.free_inodes as i64 + inodes_off) as u32;
        sb.encode(&mut raw);
        *dev.raw_mut(0) = raw;

        fs = NovaFs::mount(dev).unwrap();
        assert!(fs.repaired_counters(), "stale counters should be noticed");
        assert_eq!(fs.statfs(), truth, "and recomputed from the bitmaps");
        fs.check().unwrap();

        // The correction was written back: nothing left to repair next time.
        fs = NovaFs::mount(fs.into_device()).unwrap();
        assert!(!fs.repaired_counters());
        assert_eq!(fs.statfs(), truth);
    }
}
