//! Opening a device on Windows.
//!
//! Two things differ from a plain file here. A raw device has to be shared
//! with the rest of the system or the open is refused, and its length has to
//! be asked for with an ioctl because seeking to the end of a device handle
//! reports nothing useful. The same ioctl says how large a sector is, which is
//! the alignment every later read and write has to respect.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;

const IOCTL_DISK_GET_DRIVE_GEOMETRY: u32 = 0x0007_0000;
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405c;

// DISK_GEOMETRY, as returned by IOCTL_DISK_GET_DRIVE_GEOMETRY: an eight byte
// cylinder count, then four four-byte fields of which only the last, the
// sector size, is of any interest here.
const DISK_GEOMETRY_SIZE: usize = 24;
const DISK_GEOMETRY_BYTES_PER_SECTOR_OFFSET: usize = 20;

unsafe extern "system" {
    fn DeviceIoControl(
        device: *mut c_void,
        control_code: u32,
        in_buffer: *mut c_void,
        in_buffer_size: u32,
        out_buffer: *mut c_void,
        out_buffer_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

/// Whether the path names a device rather than a file on a filesystem.
///
/// Only used to decide how to share the handle; whether the alignment
/// machinery is needed is settled by asking the object itself.
fn looks_like_device(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    // `\\.\PhysicalDrive0`, `\\.\C:`, and the `\\?\` spelling of either. A
    // `\\?\` path naming an ordinary file is caught later, when it turns out
    // to have no disk geometry.
    matches!(path.get(..4), Some(r"\\.\") | Some(r"\\?\"))
}

pub(super) fn open(path: &Path, writable: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(writable);

    if looks_like_device(path) {
        // A device opened without sharing is refused as soon as anything else
        // in the system is holding it, which for a disk is the normal state.
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }

    options.open(path).map_err(|err| {
        if err.kind() == io::ErrorKind::PermissionDenied && looks_like_device(path) {
            io::Error::new(
                err.kind(),
                format!(
                    "{}: opening a raw device on Windows requires an elevated \
                     (\"Run as administrator\") process",
                    path.display()
                ),
            )
        } else {
            err
        }
    })
}

/// Run an ioctl that takes no input and fills `out` with a fixed-size answer.
fn ioctl(file: &File, code: u32, out: &mut [u8]) -> io::Result<()> {
    let mut returned: u32 = 0;
    // SAFETY: the handle is owned by `file` and outlives the call, and the
    // output buffer is described by its own length.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            std::ptr::null_mut(),
            0,
            out.as_mut_ptr().cast::<c_void>(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if (returned as usize) < out.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the device answered with a short ioctl reply",
        ));
    }
    Ok(())
}

/// The sector size and length of a raw device, or `None` for an image file.
///
/// An ordinary file has no disk geometry, so the ioctl failing is how a file
/// is told apart from a device: a file needs no alignment and reports its
/// length from its metadata.
pub(super) fn raw_device_geometry(_path: &Path, file: &File) -> io::Result<Option<(u64, u64)>> {
    let mut geometry = [0u8; DISK_GEOMETRY_SIZE];
    if ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY, &mut geometry).is_err() {
        return Ok(None);
    }

    let bytes_per_sector = u32::from_le_bytes(
        geometry[DISK_GEOMETRY_BYTES_PER_SECTOR_OFFSET..][..4]
            .try_into()
            .unwrap(),
    ) as u64;
    if bytes_per_sector == 0 || !bytes_per_sector.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the device reported an unusable sector size of {bytes_per_sector}"),
        ));
    }

    let mut length = [0u8; 8];
    ioctl(file, IOCTL_DISK_GET_LENGTH_INFO, &mut length)?;
    let length = u64::from_le_bytes(length);

    Ok(Some((bytes_per_sector, length)))
}
