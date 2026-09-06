//! Opening a device on macOS.
//!
//! macOS presents a disk twice, and neither node answers the ordinary file
//! calls the way Linux does. `stat` reports a size of zero, and so does
//! `lseek(SEEK_END)`, on the buffered device and on the raw one alike. The
//! length therefore has to come from an ioctl, which is the same hole Windows
//! leaves and is filled here the same way.
//!
//! Flushing differs too: `File::sync_all` asks for `F_FULLFSYNC`, which is what
//! makes a write durable on Apple hardware but which no device node
//! implements, so that has to be caught and turned back into a plain `fsync`.
//!
//! What the two nodes do not share is alignment. `/dev/diskN` is buffered:
//! the kernel serves any offset and any length out of the buffer cache, so it
//! needs a length and nothing more, and is left on the straight-through path.
//! `/dev/rdiskN` is the raw character device and rejects — `EINVAL` — anything
//! that is not a whole number of blocks starting on a block boundary, exactly
//! as a Windows device handle does, so it gets the alignment machinery.

use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

// From <sys/disk.h>: `_IOR('d', 24, uint32_t)` and `_IOR('d', 25, uint64_t)`.
// The length is the product of the two; there is no ioctl that reports it
// directly on a node this library would be pointed at.
const DKIOCGETBLOCKSIZE: c_ulong = 0x4004_6418;
const DKIOCGETBLOCKCOUNT: c_ulong = 0x4008_6419;

/// `ENOTTY`, which is what a device node answers an `fcntl` it has no notion
/// of — `F_FULLFSYNC` being one of those.
const ENOTTY: i32 = 25;

unsafe extern "C" {
    /// Declared variadic, as the C header does, so that the arguments are
    /// passed the way the platform expects; Apple silicon puts the variadic
    /// part on the stack rather than in registers.
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;

    fn fsync(fd: c_int) -> c_int;
}

pub(super) fn open(path: &Path, writable: bool) -> io::Result<File> {
    OpenOptions::new().read(true).write(writable).open(path)
}

/// Flush everything written so far.
///
/// [`File::sync_all`] issues `F_FULLFSYNC` on macOS, which asks the drive to
/// empty its own write cache and is the only thing that makes a write durable
/// across a power cut. A device node does not implement that `fcntl` at all
/// and fails it with `ENOTTY`, so every write through this library would end
/// in an error that has nothing to do with whether the write landed. Plain
/// `fsync` is what a device answers, and it is all there is to ask for: what
/// it flushes is the buffer cache, which is the only thing between the caller
/// and the media on the buffered node, and there is nothing at all on the raw
/// one.
pub(super) fn sync(file: &File) -> io::Result<()> {
    match file.sync_all() {
        Err(err) if err.raw_os_error() == Some(ENOTTY) => {
            // SAFETY: the descriptor is owned by `file` and outlives the call.
            if unsafe { fsync(file.as_raw_fd()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        other => other,
    }
}

/// Run an ioctl that takes no input and fills `out` with a fixed-size answer.
fn ioctl_read(file: &File, request: c_ulong, out: &mut [u8]) -> io::Result<()> {
    // SAFETY: the descriptor is owned by `file` and outlives the call, and
    // `out` is sized by the caller to match what the request writes back.
    let result = unsafe { ioctl(file.as_raw_fd(), request, out.as_mut_ptr().cast::<c_void>()) };

    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The alignment and length of a disk device, or `None` for an image file.
///
/// Unlike Windows, where a failing ioctl is how an image file is recognised,
/// the kind of object is settled first and from the file type: only a device
/// is asked, and a device that will not answer is an error rather than a file
/// quietly falling through.
pub(super) fn raw_device_geometry(_path: &Path, file: &File) -> io::Result<Option<(u64, u64)>> {
    let file_type = file.metadata()?.file_type();
    let raw = file_type.is_char_device();
    if !raw && !file_type.is_block_device() {
        return Ok(None);
    }

    let mut block_size = [0u8; 4];
    ioctl_read(file, DKIOCGETBLOCKSIZE, &mut block_size)?;
    let block_size = u32::from_ne_bytes(block_size) as u64;
    if block_size == 0 || !block_size.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the device reported an unusable block size of {block_size}"),
        ));
    }

    let mut block_count = [0u8; 8];
    ioctl_read(file, DKIOCGETBLOCKCOUNT, &mut block_count)?;
    let block_count = u64::from_ne_bytes(block_count);

    let len = block_size.checked_mul(block_count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the device reported an unusable length of {block_count} x {block_size}"),
        )
    })?;

    // `/dev/diskN` needs no alignment, only the length: an alignment of one
    // keeps the bounce buffer out of the way of a path the buffer cache
    // already handles.
    Ok(Some((if raw { block_size } else { 1 }, len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image file has to come back as `None`, or every image the library is
    /// pointed at would be asked for a geometry it does not have and refused.
    #[test]
    fn an_image_file_has_no_geometry() {
        let path = std::env::temp_dir().join(format!("fatx-macos-{}.img", std::process::id()));
        std::fs::write(&path, [0u8; 512]).unwrap();
        let file = File::open(&path).unwrap();

        assert!(raw_device_geometry(&path, &file).unwrap().is_none());

        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
