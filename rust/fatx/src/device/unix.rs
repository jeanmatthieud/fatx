//! Opening a device everywhere but Windows.
//!
//! Unix serves unaligned access to a block device out of the page cache and
//! reports its length through the ordinary file calls, so there is nothing to
//! work around here.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub(super) fn open(path: &Path, writable: bool) -> io::Result<File> {
    OpenOptions::new().read(true).write(writable).open(path)
}

pub(super) fn sync(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Always `None`: no path on this platform needs the alignment machinery.
pub(super) fn raw_device_geometry(_path: &Path, _file: &File) -> io::Result<Option<(u64, u64)>> {
    Ok(None)
}
