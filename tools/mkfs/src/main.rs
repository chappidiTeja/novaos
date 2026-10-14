//! Builds a NovaOS disk image from a directory tree.
//!
//!   mkfs <image> <source-dir> [--size-mib N]
//!
//! The image holds a NovaFS volume of N MiB (default 64) followed by a 1 MiB
//! scratch area that the kernel's driver self-test is free to overwrite.
//! It uses the very same filesystem code as the kernel.

use novafs::{Block, BlockDevice, FileKind, IoError, NovaFs, BLOCK_SIZE, ROOT_INO};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const SCRATCH_BLOCKS: u64 = 256;

struct FileDevice {
    file: File,
    blocks: u64,
}

impl BlockDevice for FileDevice {
    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), IoError> {
        if index >= self.blocks {
            return Err(IoError);
        }
        self.file
            .seek(SeekFrom::Start(index * BLOCK_SIZE as u64))
            .and_then(|_| self.file.read_exact(buf))
            .map_err(|_| IoError)
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), IoError> {
        if index >= self.blocks {
            return Err(IoError);
        }
        self.file
            .seek(SeekFrom::Start(index * BLOCK_SIZE as u64))
            .and_then(|_| self.file.write_all(buf))
            .map_err(|_| IoError)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.file.sync_data().map_err(|_| IoError)
    }
}

fn copy_tree(
    fs: &mut NovaFs<FileDevice>,
    dir_ino: u32,
    source: &Path,
    count: &mut (u32, u32, u64),
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(source)
        .map_err(|e| format!("{}: {}", source.display(), e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {}", source.display(), e))?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| format!("file name is not UTF-8: {:?}", n))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            let ino = fs
                .create(dir_ino, &name, FileKind::Dir)
                .map_err(|e| format!("mkdir {}: {:?}", path.display(), e))?;
            count.0 += 1;
            copy_tree(fs, ino, &path, count)?;
        } else if kind.is_file() {
            let ino = fs
                .create(dir_ino, &name, FileKind::File)
                .map_err(|e| format!("create {}: {:?}", path.display(), e))?;
            let mut source_file = File::open(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
            let mut chunk = vec![0u8; 1 << 20];
            let mut offset = 0u64;
            loop {
                let n = source_file.read(&mut chunk).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                let written = fs
                    .write(ino, offset, &chunk[..n])
                    .map_err(|e| format!("write {}: {:?}", path.display(), e))?;
                if written != n {
                    return Err(format!("{}: the volume is full", path.display()));
                }
                offset += n as u64;
            }
            count.1 += 1;
            count.2 += offset;
        } else {
            return Err(format!("{}: only files and directories are supported", path.display()));
        }
    }
    Ok(())
}

/// `mkfs --check <image>`: mount an image read-only-in-spirit and run the
/// filesystem's full consistency check.
fn check_image(image: &str) -> Result<(), String> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(image)
        .map_err(|e| format!("{}: {}", image, e))?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let device = FileDevice { file, blocks: len / BLOCK_SIZE as u64 };
    let mut fs = NovaFs::mount(device).map_err(|e| format!("cannot mount: {:?}", e))?;
    // Mounting recomputes counters left stale by a power cut; say so.
    let repaired = fs.repaired_counters();
    let report = fs.check().map_err(|e| format!("INCONSISTENT: {:?}", e))?;
    println!(
        "{}: consistent ({} dirs, {} files, {} blocks in use){}",
        image,
        report.dirs,
        report.files,
        report.blocks_in_use,
        if repaired { " [free counters were stale and have been repaired]" } else { "" }
    );
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, image] = args.as_slice() {
        if flag == "--check" {
            return check_image(image);
        }
    }
    let mut positional = Vec::new();
    let mut size_mib: u64 = 64;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--size-mib" {
            size_mib = it
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or("--size-mib needs a number")?;
        } else {
            positional.push(arg.clone());
        }
    }
    let [image, source] = positional.as_slice() else {
        return Err("usage: mkfs <image> <source-dir> [--size-mib N]".into());
    };

    let volume_blocks = size_mib * 1024 * 1024 / BLOCK_SIZE as u64;
    let total_blocks = volume_blocks + SCRATCH_BLOCKS;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image)
        .map_err(|e| format!("{}: {}", image, e))?;
    file.set_len(total_blocks * BLOCK_SIZE as u64).map_err(|e| e.to_string())?;

    let device = FileDevice { file, blocks: total_blocks };
    let mut fs = NovaFs::format(device, volume_blocks).map_err(|e| format!("format: {:?}", e))?;
    let mut count = (0u32, 0u32, 0u64);
    copy_tree(&mut fs, ROOT_INO, Path::new(source), &mut count)?;
    let report = fs.check().map_err(|e| format!("the new volume failed its own check: {:?}", e))?;
    fs.sync().map_err(|e| format!("sync: {:?}", e))?;

    let stats = fs.statfs();
    println!(
        "{}: {} MiB volume, {} dirs, {} files, {} KiB of data, {} of {} blocks free (check: {} blocks in use)",
        image,
        size_mib,
        count.0,
        count.1,
        count.2 / 1024,
        stats.free_blocks,
        stats.total_blocks,
        report.blocks_in_use
    );
    Ok(())
}

fn main() {
    if let Err(message) = run() {
        eprintln!("mkfs: {}", message);
        std::process::exit(1);
    }
}
