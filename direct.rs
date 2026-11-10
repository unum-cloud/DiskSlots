//! Positional reads and writes on files and drives, through `O_DIRECT` where the kernel offers it.
//!
//! Parts are tens of megabytes, so plain blocking `pread` and `pwrite` keep a drive busy, and the
//! copy's thread pool supplies the concurrency. Every device reports its own logical block and
//! largest transfer, and buffers are aligned to that block and lent from a pool that allocates
//! them once.

use std::fs::{File, OpenOptions};
use std::io;
use std::num::{NonZeroU16, NonZeroU32};
use std::ops::{Deref, DerefMut, Range};
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, PoisonError};

use crate::places::{Address, S3_PARTS_MAX, S3_PART_BYTES_MIN};
use crate::Error;

/// Whether a device opens for writing too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// A file or a drive open for positional transfers.
pub struct Device {
    file: File,
    pub path: PathBuf,
    pub bytes: u64,
    /// The unit of every offset, length and buffer address in a direct transfer.
    pub logical_block: NonZeroU32,
    /// The most bytes one command moves: a drive's queue limit, a file's preferred I/O size.
    pub transfer_bytes: NonZeroU32,
    pub drive: bool,
}

/// What a device reports about its size and transfers.
struct Geometry {
    bytes: u64,
    logical_block: NonZeroU32,
    transfer_bytes: NonZeroU32,
}

impl Device {
    /// Opens a file or a drive; a drive opened for writing is opened exclusively, so a mounted one
    /// refuses.
    pub fn open(path: &Path, access: Access) -> Result<Device, Error> {
        let failed = |source| Error::Io {
            address: Address::path(path),
            source,
        };
        let drive = std::fs::metadata(path)
            .map_err(failed)?
            .file_type()
            .is_block_device();
        let mut options = OpenOptions::new();
        options.read(true).write(access == Access::Write);
        let exclusive = drive && access == Access::Write;
        let (file, direct) = open_direct(&options, path, exclusive).map_err(failed)?;
        let geometry = match drive {
            true => drive_geometry(&file, path),
            false => file_geometry(&file, direct),
        }
        .map_err(|query| Error::Geometry {
            address: Address::path(path),
            query,
        })?;
        Ok(Device {
            file,
            path: path.to_path_buf(),
            bytes: geometry.bytes,
            logical_block: geometry.logical_block,
            transfer_bytes: geometry.transfer_bytes,
            drive,
        })
    }

    /// Creates a new file, refusing one that already exists.
    pub fn create(path: &Path) -> Result<Device, Error> {
        // Created buffered first: a filesystem refusing `O_DIRECT` may already have made the file.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| Error::Io {
                address: Address::path(path),
                source,
            })?;
        Device::open(path, Access::Write)
    }

    /// The address of `extent` on this device, which errors about it carry.
    pub fn address(&self, extent: Range<u64>) -> Address {
        Address::extent(&self.path, extent)
    }

    fn failed(&self, extent: Range<u64>) -> impl FnOnce(io::Error) -> Error + '_ {
        move |source| Error::Io {
            address: self.address(extent),
            source,
        }
    }

    /// Reads into `buffer` from `offset` until it is full or the file ends, returning the bytes read.
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize, Error> {
        let mut filled = 0;
        while filled < buffer.len() {
            match self
                .file
                .read_at(&mut buffer[filled..], offset + filled as u64)
            {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(self.failed(offset..offset + buffer.len() as u64)(source))
                }
            }
        }
        Ok(filled)
    }

    pub fn write_at(&self, buffer: &[u8], offset: u64) -> Result<(), Error> {
        let extent = offset..offset + buffer.len() as u64;
        self.file
            .write_all_at(buffer, offset)
            .map_err(self.failed(extent))
    }

    pub fn sync(&self) -> Result<(), Error> {
        self.file.sync_all().map_err(self.failed(0..0))
    }

    pub fn set_len(&self, bytes: u64) -> Result<(), Error> {
        self.file.set_len(bytes).map_err(self.failed(0..0))
    }

    /// Hands `extent` back to the drive where the kernel can, as on a Linux drive.
    pub fn discard(&self, extent: Range<u64>) -> Result<(), Error> {
        if !self.drive {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            /// `_IO(0x12, 119)` in `<linux/fs.h>`.
            const BLKDISCARD: libc::Ioctl = 0x1277;
            let range = [extent.start, extent.end - extent.start];
            // SAFETY: the kernel reads two `u64`s from `range`, which lives across the call.
            if unsafe { libc::ioctl(self.file.as_raw_fd(), BLKDISCARD, range.as_ptr()) } != 0 {
                return Err(self.failed(extent)(io::Error::last_os_error()));
            }
        }
        let _ = extent;
        Ok(())
    }
}

impl AsRawFd for Device {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// Opens through `O_DIRECT` where the filesystem takes it, saying whether it did.
#[cfg(target_os = "linux")]
fn open_direct(options: &OpenOptions, path: &Path, exclusive: bool) -> io::Result<(File, bool)> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut direct = options.clone();
    direct.custom_flags(libc::O_DIRECT | if exclusive { libc::O_EXCL } else { 0 });
    match direct.open(path) {
        Ok(file) => Ok((file, true)),
        // tmpfs and some network filesystems refuse `O_DIRECT`; the buffered path moves the same bytes.
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
            Ok((options.open(path)?, false))
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn open_direct(options: &OpenOptions, path: &Path, _exclusive: bool) -> io::Result<(File, bool)> {
    Ok((options.open(path)?, false))
}

fn nonzero(value: u64, query: &'static str) -> Result<NonZeroU32, &'static str> {
    u32::try_from(value)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or(query)
}

/// A logical block, which every direct transfer aligns to and so must be a power of two.
fn block(value: u64, query: &'static str) -> Result<NonZeroU32, &'static str> {
    nonzero(value, query)?
        .is_power_of_two()
        .then(|| nonzero(value, query))
        .ok_or(query)?
}

/// A constant of the running system, or `query` when it reports none.
fn sysconf(name: libc::c_int, query: &'static str) -> Result<u64, &'static str> {
    // SAFETY: `sysconf` reads a constant of the running system.
    u64::try_from(unsafe { libc::sysconf(name) }).map_err(|_| query)
}

#[cfg(target_os = "linux")]
fn drive_geometry(mut file: &File, path: &Path) -> Result<Geometry, &'static str> {
    use std::io::{Seek, SeekFrom};
    /// `queue/max_sectors_kb` counts KiB.
    const MAX_SECTORS_UNIT: u64 = 1 << 10;
    const MAX_SECTORS: &str = "queue/max_sectors_kb";
    let mut logical_block: libc::c_int = 0;
    // SAFETY: `BLKSSZGET` writes one `int` into `logical_block`, which lives across the call.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::BLKSSZGET, &mut logical_block) } != 0 {
        return Err("BLKSSZGET");
    }
    // A partition shares the block queue of its disk.
    let real = std::fs::canonicalize(path).map_err(|_| MAX_SECTORS)?;
    let class = Path::new("/sys/class/block").join(real.file_name().ok_or(MAX_SECTORS)?);
    let transfer_kib: u64 = [class.join(MAX_SECTORS), class.join("..").join(MAX_SECTORS)]
        .iter()
        .find_map(|queue| std::fs::read_to_string(queue).ok()?.trim().parse().ok())
        .ok_or(MAX_SECTORS)?;
    Ok(Geometry {
        bytes: file.seek(SeekFrom::End(0)).map_err(|_| "SEEK_END")?,
        logical_block: block(
            u64::try_from(logical_block).map_err(|_| "BLKSSZGET")?,
            "BLKSSZGET",
        )?,
        transfer_bytes: nonzero(transfer_kib.saturating_mul(MAX_SECTORS_UNIT), MAX_SECTORS)?,
    })
}

#[cfg(target_os = "linux")]
fn file_geometry(file: &File, direct: bool) -> Result<Geometry, &'static str> {
    // SAFETY: `statx` is plain data the call fills.
    let mut status: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: an empty path with `AT_EMPTY_PATH` names `file` itself, and `status` outlives the call.
    let returned = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_SIZE | libc::STATX_DIOALIGN,
            &mut status,
        )
    };
    if returned != 0 || status.stx_mask & libc::STATX_SIZE == 0 {
        return Err("statx");
    }
    let logical_block = match direct {
        // Buffered transfers take any alignment, so the filesystem's own block serves.
        false => block(status.stx_blksize.into(), "stx_blksize")?,
        true if status.stx_mask & libc::STATX_DIOALIGN == 0 => return Err("STATX_DIOALIGN"),
        true => block(
            status
                .stx_dio_offset_align
                .max(status.stx_dio_mem_align)
                .into(),
            "STATX_DIOALIGN",
        )?,
    };
    Ok(Geometry {
        bytes: status.stx_size,
        logical_block,
        transfer_bytes: nonzero(status.stx_blksize.into(), "stx_blksize")?,
    })
}

/// `_IOR('d', 24, uint32_t)` in `<sys/disk.h>`.
#[cfg(target_os = "macos")]
const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;

/// `_IOR('d', 25, uint64_t)` in `<sys/disk.h>`.
#[cfg(target_os = "macos")]
const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;

/// `_IOR('d', 70, uint64_t)` in `<sys/disk.h>`.
#[cfg(target_os = "macos")]
const DKIOCGETMAXBYTECOUNTREAD: libc::c_ulong = 0x4008_6446;

/// The value a `<sys/disk.h>` ioctl writes, or `query` when the device refuses it.
#[cfg(target_os = "macos")]
fn disk_query<Value: Default>(
    file: &File,
    request: libc::c_ulong,
    query: &'static str,
) -> Result<Value, &'static str> {
    let mut value = Value::default();
    // SAFETY: each request this module sends writes one `Value` it names, which outlives the call.
    match unsafe { libc::ioctl(file.as_raw_fd(), request, &mut value as *mut Value) } {
        0 => Ok(value),
        _ => Err(query),
    }
}

#[cfg(target_os = "macos")]
fn drive_geometry(file: &File, _path: &Path) -> Result<Geometry, &'static str> {
    let logical_block: u32 = disk_query(file, DKIOCGETBLOCKSIZE, "DKIOCGETBLOCKSIZE")?;
    let blocks: u64 = disk_query(file, DKIOCGETBLOCKCOUNT, "DKIOCGETBLOCKCOUNT")?;
    let transfer: u64 = disk_query(file, DKIOCGETMAXBYTECOUNTREAD, "DKIOCGETMAXBYTECOUNTREAD")?;
    Ok(Geometry {
        bytes: blocks
            .checked_mul(logical_block.into())
            .ok_or("DKIOCGETBLOCKCOUNT")?,
        logical_block: block(logical_block.into(), "DKIOCGETBLOCKSIZE")?,
        transfer_bytes: nonzero(transfer, "DKIOCGETMAXBYTECOUNTREAD")?,
    })
}

#[cfg(target_os = "macos")]
fn file_geometry(file: &File, _direct: bool) -> Result<Geometry, &'static str> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|_| "fstat")?;
    Ok(Geometry {
        bytes: metadata.len(),
        logical_block: block(sysconf(libc::_SC_PAGESIZE, "_SC_PAGESIZE")?, "_SC_PAGESIZE")?,
        transfer_bytes: nonzero(metadata.blksize(), "st_blksize")?,
    })
}

/// The physical memory this process may use: the machine's, or a smaller cgroup limit on Linux.
pub fn memory_bytes() -> Result<u64, &'static str> {
    let pages = sysconf(libc::_SC_PHYS_PAGES, "_SC_PHYS_PAGES")?;
    let machine = pages.saturating_mul(sysconf(libc::_SC_PAGESIZE, "_SC_PAGESIZE")?);
    #[cfg(target_os = "linux")]
    if let Some(limit) = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|limit| limit.trim().parse::<u64>().ok())
    {
        return Ok(machine.min(limit));
    }
    Ok(machine)
}

/// Bytes per part of a `bytes`-long transfer through `device`: no fewer than S3 takes, few enough
/// parts for one upload, at least one command of the device, and a power of two, so parts tile slots.
pub fn part_bytes(bytes: u64, device: &Device) -> u64 {
    S3_PART_BYTES_MIN
        .max(bytes.div_ceil(u64::from(S3_PARTS_MAX)))
        .max(u64::from(device.transfer_bytes.get()))
        .next_power_of_two()
}

/// Total bytes of the filesystem holding `path`.
pub fn filesystem_bytes(path: &Path) -> Result<u64, Error> {
    use std::os::unix::ffi::OsStrExt;
    let failed = |source| Error::Io {
        address: Address::path(path),
        source,
    };
    let text = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| failed(io::ErrorKind::InvalidInput.into()))?;
    // SAFETY: `statvfs` is plain data the call fills.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `text` is NUL-terminated and `stats` outlives the call.
    if unsafe { libc::statvfs(text.as_ptr(), &mut stats) } != 0 {
        return Err(failed(io::Error::last_os_error()));
    }
    // `fsblkcnt_t` is 32 bits on macOS and 64 on Linux.
    #[allow(clippy::useless_conversion)]
    u64::from(stats.f_blocks)
        .checked_mul(u64::from(stats.f_frsize))
        .ok_or_else(|| failed(io::ErrorKind::InvalidData.into()))
}

/// Zeroed bytes at a device's logical block alignment, fit for its direct transfers.
#[derive(Default)]
pub struct AlignedBuffer {
    /// One logical block longer than the buffer, so an aligned start lies inside it.
    storage: Vec<u8>,
    start: usize,
    length: usize,
}

impl AlignedBuffer {
    /// Allocates `bytes` zeroed bytes for `device`, failing rather than aborting when memory is short.
    pub fn zeroed(device: &Device, bytes: u64) -> Result<AlignedBuffer, Error> {
        let out_of_memory = || Error::Io {
            address: device.address(0..bytes),
            source: io::ErrorKind::OutOfMemory.into(),
        };
        let alignment = device.logical_block.get() as usize;
        let length = usize::try_from(bytes).map_err(|_| out_of_memory())?;
        let reserved = length.checked_add(alignment).ok_or_else(out_of_memory)?;
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(reserved)
            .map_err(|_| out_of_memory())?;
        storage.resize(reserved, 0);
        let start = storage.as_ptr().align_offset(alignment);
        Ok(AlignedBuffer {
            storage,
            start,
            length,
        })
    }
}

impl Deref for AlignedBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.storage[self.start..][..self.length]
    }
}

impl DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..][..self.length]
    }
}

/// Buffers of one size allocated once, lent out and returned on drop, so warm transfers allocate
/// nothing.
pub struct BufferPool {
    returned: Mutex<Receiver<AlignedBuffer>>,
    returning: SyncSender<AlignedBuffer>,
}

/// A buffer lent from a [`BufferPool`], showing its first `length` bytes, which goes back on drop.
pub struct PooledBuffer {
    buffer: AlignedBuffer,
    length: usize,
    returning: SyncSender<AlignedBuffer>,
}

impl BufferPool {
    /// Allocates `buffers` zeroed buffers of `buffer_bytes` each for `device`'s transfers.
    pub fn new(
        device: &Device,
        buffers: NonZeroU16,
        buffer_bytes: u64,
    ) -> Result<BufferPool, Error> {
        let (returning, returned) = mpsc::sync_channel(buffers.get().into());
        for _ in 0..buffers.get() {
            returning
                .try_send(AlignedBuffer::zeroed(device, buffer_bytes)?)
                .expect("the channel holds every buffer of the pool");
        }
        Ok(BufferPool {
            returned: Mutex::new(returned),
            returning,
        })
    }

    /// The next buffer to come back, waiting while every one is lent out.
    pub fn take(&self) -> PooledBuffer {
        let returned = self.returned.lock().unwrap_or_else(PoisonError::into_inner);
        let buffer = returned.recv().expect("the pool holds a sender");
        PooledBuffer {
            length: buffer.len(),
            buffer,
            returning: self.returning.clone(),
        }
    }
}

impl PooledBuffer {
    /// Shows the first `length` bytes, at most the whole allocation.
    pub fn set_len(&mut self, length: usize) {
        assert!(length <= self.buffer.len(), "a pooled buffer never grows");
        self.length = length;
    }
}

impl AsRef<[u8]> for PooledBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.buffer[..self.length]
    }
}

impl AsMut<[u8]> for PooledBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buffer[..self.length]
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        // The pool's channel holds every buffer, so only a dropped pool refuses one, freeing it.
        let _ = self.returning.try_send(std::mem::take(&mut self.buffer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_file() -> (tempfile::NamedTempFile, Device) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let device = Device::open(file.path(), Access::Write).unwrap();
        (file, device)
    }

    #[test]
    fn part_bytes_stays_within_the_s3_limits() {
        let (_file, device) = scratch_file();
        let transfer = u64::from(device.transfer_bytes.get());
        let smallest = S3_PART_BYTES_MIN.max(transfer).next_power_of_two();
        let parts_max = u64::from(S3_PARTS_MAX);
        assert_eq!(part_bytes(0, &device), smallest);
        assert_eq!(part_bytes(parts_max * smallest, &device), smallest);
        assert_eq!(part_bytes(parts_max * smallest + 1, &device), smallest * 2);
        for bytes in [
            1,
            S3_PART_BYTES_MIN,
            parts_max * S3_PART_BYTES_MIN + 1,
            u64::MAX,
        ] {
            let part = part_bytes(bytes, &device);
            assert!(part >= S3_PART_BYTES_MIN && part >= transfer && part.is_power_of_two());
            assert!(
                bytes.div_ceil(part) <= parts_max,
                "{bytes} bytes in parts of {part}"
            );
        }
    }

    #[test]
    fn pool_lends_the_same_allocations_again() {
        let (_file, device) = scratch_file();
        let pool = BufferPool::new(&device, NonZeroU16::new(2).unwrap(), 1 << 16).unwrap();
        let address = |buffer: &PooledBuffer| buffer.as_ref().as_ptr() as usize;
        let mut first = pool.take();
        let second = pool.take();
        let mut allocations = [address(&first), address(&second)];
        allocations.sort();
        let block = device.logical_block.get() as usize;
        assert!(allocations.iter().all(|&at| at % block == 0));

        first.set_len(100);
        assert_eq!(first.as_ref().len(), 100);
        let body = bytes::Bytes::from_owner(first);
        assert_eq!(body.len(), 100);
        std::thread::spawn(move || drop((body, second)))
            .join()
            .unwrap();

        let again = [pool.take(), pool.take()];
        assert!(again.iter().all(|buffer| buffer.as_ref().len() == 1 << 16));
        let mut reused = [address(&again[0]), address(&again[1])];
        reused.sort();
        assert_eq!(reused, allocations);
    }
}
