//! The block device, or image file, a filesystem lives on.
//!
//! FATX asks the device for whatever it happens to need: 4096 bytes of
//! superblock, a 64 byte directory entry, or the single byte that marks an
//! entry deleted. Unix is happy to serve any of those from a block device, so
//! the driver used to hand its reads and writes straight to [`std::fs::File`].
//!
//! Windows is not. A handle on a raw device rejects any access that does not
//! start on a sector boundary and cover whole sectors, and it cannot report
//! its length through the ordinary file calls either. macOS splits the two
//! problems across its two nodes for the same disk: `/dev/diskN` takes any
//! access but reports no length, `/dev/rdiskN` reports no length and takes
//! only aligned whole blocks. This module papers over all of it, so the rest
//! of the library keeps asking for the bytes it wants and stays free of
//! `#[cfg]`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

#[cfg(windows)]
#[path = "device/windows.rs"]
mod sys;

#[cfg(target_os = "macos")]
#[path = "device/macos.rs"]
mod sys;

#[cfg(not(any(windows, target_os = "macos")))]
#[path = "device/unix.rs"]
mod sys;

/// One sector, as it was last read.
#[derive(Debug)]
struct CachedSector {
    offset: u64,
    data: Vec<u8>,
    /// How much of `data` came off the device; the rest is zero padding for a
    /// sector that runs past the end of it.
    valid: usize,
}

/// A device the filesystem is read from and written to.
#[derive(Debug)]
pub(crate) struct Device {
    file: File,
    /// The alignment every access to `file` has to satisfy, in bytes.
    ///
    /// One means the platform accepts any offset and any length, which is the
    /// case for image files everywhere, for block devices outside Windows, and
    /// for the buffered `/dev/diskN` on macOS; the bounce buffer is then
    /// skipped entirely. Note that an alignment of one says nothing about the
    /// length: a macOS block device sets this to one and still supplies `len`.
    sector_size: u64,
    /// Where the *caller* is positioned, which need not be aligned at all.
    pos: u64,
    /// The device length, when it took a platform call to find out. Image
    /// files answer from their metadata instead, which stays correct as they
    /// grow.
    len: Option<u64>,
    /// The last sector read, kept because a directory is walked one 64 byte
    /// entry at a time and would otherwise read the same sector eight times
    /// over. Dropped on every write rather than working out which writes could
    /// have invalidated it.
    cache: Option<CachedSector>,
}

impl Device {
    /// Open a device or image file.
    pub(crate) fn open<P: AsRef<Path>>(path: P, writable: bool) -> io::Result<Self> {
        let path = path.as_ref();
        let file = sys::open(path, writable)?;

        let (sector_size, len) = match sys::raw_device_geometry(path, &file)? {
            Some((sector_size, len)) => (sector_size, Some(len)),
            None => (1, None),
        };

        Ok(Self {
            file,
            sector_size,
            pos: 0,
            len,
            cache: None,
        })
    }

    /// Open a file and treat it as a device of the given sector size.
    ///
    /// The alignment path is otherwise only reachable on Windows and only with
    /// a real disk plugged in, which is no way to keep it honest.
    #[cfg(test)]
    fn open_aligned<P: AsRef<Path>>(path: P, sector_size: u64) -> io::Result<Self> {
        Ok(Self {
            file: std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.as_ref())?,
            sector_size,
            pos: 0,
            len: None,
            cache: None,
        })
    }

    /// The length of the device in bytes.
    ///
    /// Seeking to the end is what answers for the objects left to this path: a
    /// block device reports a size of zero from `stat`, so its metadata cannot
    /// be asked, while an image file has no geometry to query. Windows and
    /// macOS, where a device answers neither `stat` nor `lseek(SEEK_END)`, are
    /// why the platform layer gets to supply the length instead.
    ///
    /// The underlying position is left wherever this leaves it; every read and
    /// write seeks first, and the caller's own position lives in `pos`.
    pub(crate) fn len(&mut self) -> io::Result<u64> {
        match self.len {
            Some(len) => Ok(len),
            None => self.file.seek(SeekFrom::End(0)),
        }
    }

    pub(crate) fn sync_all(&mut self) -> io::Result<()> {
        sys::sync(&self.file)
    }

    /// Whether accesses have to be aligned, i.e. whether the bounce buffer is
    /// in play at all.
    fn aligned(&self) -> bool {
        self.sector_size > 1
    }

    /// Read whole sectors starting at `offset`, which must itself be aligned.
    ///
    /// Reports how many bytes were actually read: a read running off the end
    /// of the device comes back short rather than failing, which is what the
    /// callers above expect of an entry claiming more bytes than the device
    /// holds.
    fn read_sectors_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        debug_assert!(offset.is_multiple_of(self.sector_size));
        debug_assert!((buf.len() as u64).is_multiple_of(self.sector_size));

        self.file.seek(SeekFrom::Start(offset))?;
        let mut done = 0;
        while done < buf.len() {
            match self.file.read(&mut buf[done..]) {
                Ok(0) => break,
                Ok(read) => done += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(done)
    }

    /// Read the single sector at `offset` into `buf`, through the cache.
    ///
    /// Reports how much of `buf` actually came off the device, as
    /// [`Self::read_sectors_at`] does; the rest is zeroed, so a
    /// read-modify-write of a sector that runs past the end still does the
    /// right thing.
    fn read_cached_sector(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        debug_assert_eq!(buf.len() as u64, self.sector_size);

        if let Some(cached) = &self.cache
            && cached.offset == offset
        {
            buf.copy_from_slice(&cached.data);
            return Ok(cached.valid);
        }

        let valid = self.read_sectors_at(offset, buf)?;
        buf[valid..].fill(0);
        self.cache = Some(CachedSector {
            offset,
            data: buf.to_vec(),
            valid,
        });
        Ok(valid)
    }
}

impl Read for Device {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if !self.aligned() {
            self.file.seek(SeekFrom::Start(self.pos))?;
            let read = self.file.read(buf)?;
            self.pos += read as u64;
            return Ok(read);
        }

        let sector_size = self.sector_size;
        let begin = self.pos;
        let end = begin + buf.len() as u64;
        let aligned_begin = begin - begin % sector_size;
        let aligned_end = end.next_multiple_of(sector_size);
        let skip = (begin - aligned_begin) as usize;

        let mut scratch = vec![0u8; (aligned_end - aligned_begin) as usize];
        let read = if scratch.len() as u64 == sector_size {
            self.read_cached_sector(aligned_begin, &mut scratch)?
        } else {
            self.read_sectors_at(aligned_begin, &mut scratch)?
        };

        let taken = read.saturating_sub(skip).min(buf.len());
        buf[..taken].copy_from_slice(&scratch[skip..skip + taken]);
        self.pos += taken as u64;
        Ok(taken)
    }
}

impl Write for Device {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if !self.aligned() {
            self.file.seek(SeekFrom::Start(self.pos))?;
            let written = self.file.write(buf)?;
            self.pos += written as u64;
            return Ok(written);
        }

        // Whatever the write does not cover in full has to be read back first,
        // or the rest of those sectors would be replaced with zeroes. Only the
        // first and the last sector can be partial, so only those are read.
        let sector_size = self.sector_size;
        let sector = sector_size as usize;
        let begin = self.pos;
        let end = begin + buf.len() as u64;
        let aligned_begin = begin - begin % sector_size;
        let aligned_end = end.next_multiple_of(sector_size);
        let head = (begin - aligned_begin) as usize;
        let tail = (aligned_end - end) as usize;
        let last = (aligned_end - sector_size - aligned_begin) as usize;

        let mut scratch = vec![0u8; (aligned_end - aligned_begin) as usize];
        if head != 0 {
            self.read_cached_sector(aligned_begin, &mut scratch[..sector])?;
        }
        // When the write lives inside a single sector, the head read above has
        // already brought that sector in.
        if tail != 0 && !(head != 0 && last == 0) {
            self.read_cached_sector(aligned_end - sector_size, &mut scratch[last..])?;
        }

        scratch[head..head + buf.len()].copy_from_slice(buf);

        self.cache = None;
        self.file.seek(SeekFrom::Start(aligned_begin))?;
        self.file.write_all(&scratch)?;
        self.pos = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for Device {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
            SeekFrom::End(offset) => self.len()? as i64 + offset,
        };

        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the device",
            ));
        }

        self.pos = target as u64;
        Ok(self.pos)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR: u64 = 512;

    /// A scratch file holding `len` bytes of a recognisable pattern, so that
    /// any byte the alignment layer disturbs by mistake shows up as the wrong
    /// value rather than as a zero that might have been there anyway.
    struct Scratch {
        path: std::path::PathBuf,
        expected: Vec<u8>,
    }

    impl Scratch {
        fn new(name: &str, len: usize) -> Self {
            let expected: Vec<u8> = (0..len).map(|index| (index % 251) as u8).collect();
            let path =
                std::env::temp_dir().join(format!("fatx-device-{name}-{}.img", std::process::id()));
            std::fs::write(&path, &expected).unwrap();
            Self { path, expected }
        }

        fn device(&self) -> Device {
            Device::open_aligned(&self.path, SECTOR).unwrap()
        }

        /// What the file holds now, which is what the next process to open it
        /// would see.
        fn on_disk(&self) -> Vec<u8> {
            std::fs::read(&self.path).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn an_unaligned_read_returns_the_bytes_that_were_asked_for() {
        let scratch = Scratch::new("read", 8 * SECTOR as usize);
        let mut device = scratch.device();

        // Inside one sector, across a sector boundary, and spanning several.
        for (offset, len) in [(0u64, 1usize), (37, 64), (500, 24), (100, 2000)] {
            device.seek(SeekFrom::Start(offset)).unwrap();
            let mut buf = vec![0u8; len];
            device.read_exact(&mut buf).unwrap();
            assert_eq!(
                buf,
                scratch.expected[offset as usize..offset as usize + len],
                "reading {len} bytes at {offset}"
            );
            assert_eq!(device.stream_position().unwrap(), offset + len as u64);
        }
    }

    #[test]
    fn an_unaligned_write_leaves_the_rest_of_the_sector_alone() {
        let scratch = Scratch::new("write", 8 * SECTOR as usize);
        let mut expected = scratch.expected.clone();

        for (offset, len) in [(0u64, 1usize), (37, 64), (500, 24), (100, 2000)] {
            let payload: Vec<u8> = (0..len).map(|index| (index as u8) ^ 0xa5).collect();

            let mut device = scratch.device();
            device.seek(SeekFrom::Start(offset)).unwrap();
            device.write_all(&payload).unwrap();
            drop(device);

            expected[offset as usize..offset as usize + len].copy_from_slice(&payload);
            assert_eq!(
                scratch.on_disk(),
                expected,
                "writing {len} bytes at {offset}"
            );
        }
    }

    #[test]
    fn a_read_after_a_write_sees_the_new_bytes() {
        let scratch = Scratch::new("cache", 4 * SECTOR as usize);
        let mut device = scratch.device();

        // Fill the cache with the sector about to be written, so a stale entry
        // would be there to be served.
        let mut before = [0u8; 16];
        device.seek(SeekFrom::Start(0)).unwrap();
        device.read_exact(&mut before).unwrap();

        device.seek(SeekFrom::Start(4)).unwrap();
        device.write_all(&[0xde, 0xad, 0xbe, 0xef]).unwrap();

        let mut after = [0u8; 16];
        device.seek(SeekFrom::Start(0)).unwrap();
        device.read_exact(&mut after).unwrap();

        assert_eq!(&after[4..8], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(&after[..4], &before[..4]);
        assert_eq!(&after[8..], &before[8..]);
    }

    #[test]
    fn a_read_running_off_the_end_comes_back_short() {
        let scratch = Scratch::new("short", SECTOR as usize + 100);
        let mut device = scratch.device();

        device.seek(SeekFrom::Start(SECTOR + 90)).unwrap();
        let mut buf = [0u8; 64];
        let read = device.read(&mut buf).unwrap();

        assert_eq!(read, 10);
        assert_eq!(&buf[..10], &scratch.expected[SECTOR as usize + 90..]);

        device.seek(SeekFrom::Start(SECTOR + 100)).unwrap();
        assert_eq!(device.read(&mut buf).unwrap(), 0);
    }
}
