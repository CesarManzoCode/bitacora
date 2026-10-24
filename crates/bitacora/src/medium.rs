//! Storage medium: the byte-addressed device a volume lives on.
//!
//! `FileMedium` is the real implementation over an ordinary file. The
//! simulated disk in `bitacora-verify` implements the same trait.

use std::alloc::{self, Layout};
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

use crate::error::{Error, Result};

/// Alignment required for `O_DIRECT` buffers, offsets and lengths.
pub const DIRECT_ALIGN: usize = 4096;

/// A byte-addressed storage device.
pub trait Medium: Send + Sync {
    /// Reads into `buf` starting at `offset`. Returns the number of bytes
    /// read, which is smaller than `buf.len()` only when the end of the
    /// medium is reached.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Writes all of `buf` starting at `offset`, growing the medium if needed.
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()>;

    /// Makes every completed write durable (`fdatasync`).
    fn sync(&self) -> Result<()>;

    /// Current length of the medium in bytes.
    fn len(&self) -> Result<u64>;

    /// Whether the medium is empty.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Releases the space of `[offset, offset + len)` without changing the
    /// length; the range then reads as zeros.
    fn punch_hole(&self, offset: u64, len: u64) -> Result<()>;
}

/// Heap buffer aligned to `DIRECT_ALIGN`, zero-initialised.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: `AlignedBuf` owns its allocation exclusively, like `Vec<u8>`.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only hands out `&[u8]`.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    pub fn zeroed(len: usize) -> Self {
        let size = len.max(1).next_multiple_of(DIRECT_ALIGN);
        let layout = Layout::from_size_align(size, DIRECT_ALIGN).expect("valid aligned layout");
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        AlignedBuf { ptr, len, layout }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to at least `len` initialised bytes.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to at least `len` initialised bytes we own.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `zeroed` with this same layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// A medium backed by an ordinary file, using `pread`/`pwrite`,
/// `fdatasync` and `fallocate`.
pub struct FileMedium {
    file: File,
    direct: bool,
}

impl FileMedium {
    /// Creates the file (truncating an existing one) and opens it.
    pub fn create(path: &Path, direct: bool) -> Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(true);
        Self::open_with(opts, path, direct)
    }

    /// Opens an existing file.
    pub fn open(path: &Path, direct: bool) -> Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        Self::open_with(opts, path, direct)
    }

    fn open_with(mut opts: OpenOptions, path: &Path, direct: bool) -> Result<Self> {
        if direct {
            opts.custom_flags(libc::O_DIRECT);
        }
        let file = opts.open(path)?;
        Ok(FileMedium { file, direct })
    }

    /// Whether the file was opened with `O_DIRECT`.
    pub fn is_direct(&self) -> bool {
        self.direct
    }

    fn check_direct(&self, offset: u64, len: usize) -> Result<()> {
        if self.direct
            && (!offset.is_multiple_of(DIRECT_ALIGN as u64) || !len.is_multiple_of(DIRECT_ALIGN))
        {
            return Err(Error::InvalidArgument(format!(
                "O_DIRECT access must be {DIRECT_ALIGN}-aligned: offset {offset}, length {len}"
            )));
        }
        Ok(())
    }

    fn needs_bounce(&self, ptr: *const u8) -> bool {
        self.direct && !(ptr as usize).is_multiple_of(DIRECT_ALIGN)
    }

    fn pread_full(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Ok(done)
    }

    fn pwrite_full(&self, buf: &[u8], offset: u64) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.write_at(&buf[done..], offset + done as u64) {
                Ok(0) => return Err(Error::Io(io::Error::from(io::ErrorKind::WriteZero))),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Ok(())
    }
}

impl Medium for FileMedium {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.check_direct(offset, buf.len())?;
        if self.needs_bounce(buf.as_ptr()) {
            let mut bounce = AlignedBuf::zeroed(buf.len());
            let n = self.pread_full(&mut bounce, offset)?;
            buf[..n].copy_from_slice(&bounce[..n]);
            return Ok(n);
        }
        self.pread_full(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        self.check_direct(offset, buf.len())?;
        if self.needs_bounce(buf.as_ptr()) {
            let mut bounce = AlignedBuf::zeroed(buf.len());
            bounce.copy_from_slice(buf);
            return self.pwrite_full(&bounce, offset);
        }
        self.pwrite_full(buf, offset)
    }

    fn sync(&self) -> Result<()> {
        // SAFETY: the descriptor is owned by `self.file` and open.
        let rc = unsafe { libc::fdatasync(self.file.as_raw_fd()) };
        if rc != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn punch_hole(&self, offset: u64, len: u64) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let off = libc::off_t::try_from(offset)
            .map_err(|_| Error::InvalidArgument(format!("offset {offset} out of range")))?;
        let n = libc::off_t::try_from(len)
            .map_err(|_| Error::InvalidArgument(format!("length {len} out of range")))?;
        // SAFETY: the descriptor is owned by `self.file` and open.
        let rc = unsafe {
            libc::fallocate(
                self.file.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                off,
                n,
            )
        };
        if rc != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
}
