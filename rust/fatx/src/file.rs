use std::cmp::min;
use std::io;

use crate::datetime::DateTime;
use crate::dir::{DirectoryEntry, EntryLocation};
use crate::error::Error;
use crate::fat::ClusterId;
use crate::fs::{FatxFs, FatxFsHandle};

/// An open file.
///
/// Each handle carries its own copy of the directory entry and writes the whole
/// of it back after a write, so two handles open on the same path will overwrite
/// each other's idea of the file's size. Open a path once at a time.
pub struct File {
    handle: FatxFsHandle,
    dirent: DirectoryEntry,
    /// Where the entry describing this file lives, so that its size and
    /// timestamps can be written back. The root directory has no entry, but a
    /// File is only ever opened on a regular file, which always has one.
    location: Option<EntryLocation>,
    cur_cluster_relative: u32,
    cur_cluster_absolute: ClusterId,
    seek_pos: u32,
}

impl File {
    pub(crate) fn new(
        handle: FatxFsHandle,
        dirent: DirectoryEntry,
        location: Option<EntryLocation>,
    ) -> Self {
        Self {
            handle,
            seek_pos: 0,
            cur_cluster_relative: 0,
            cur_cluster_absolute: dirent.first_cluster(),
            location,
            dirent,
        }
    }

    pub fn file_size(&self) -> u32 {
        self.dirent.file_size()
    }

    /// Map the current file position to a cluster, walking the chain from
    /// wherever the last access left off.
    ///
    /// With `alloc` set, the chain is extended to reach the position, which is
    /// what writing past the last cluster of a file needs.
    fn cluster_at_seek_pos(&mut self, fs: &mut FatxFs, alloc: bool) -> Result<ClusterId, Error> {
        let target_relative = (self.seek_pos as u64 / fs.num_bytes_per_cluster) as u32;

        // Check if we need to begin scanning from start of cluster chain
        if target_relative < self.cur_cluster_relative {
            self.cur_cluster_relative = 0;
            self.cur_cluster_absolute = self.dirent.first_cluster();
        }

        // Scan through the cluster chain
        for _ in self.cur_cluster_relative..target_relative {
            self.cur_cluster_absolute = match fs.next_cluster(self.cur_cluster_absolute)? {
                Some(cluster) => cluster,
                None if alloc => {
                    let cluster = fs.alloc_cluster(true)?;
                    fs.attach_cluster(self.cur_cluster_absolute, cluster)?;
                    cluster
                }
                None => return Err(Error::InvalidClusterChain),
            };
            self.cur_cluster_relative += 1;
        }

        Ok(self.cur_cluster_absolute)
    }

    /// Grow the file to `new_size`, backing the new space with zeroes.
    ///
    /// Only the size held in the entry is updated; it reaches the disk when the
    /// entry is written back.
    fn extend_to(&mut self, fs: &mut FatxFs, new_size: u64) -> Result<(), Error> {
        let old_size = self.dirent.file_size() as u64;
        if new_size <= old_size {
            return Ok(());
        }

        let first_cluster = self.dirent.first_cluster();
        fs.resize_chain(first_cluster, old_size, new_size)?;

        // Resizing may have dropped clusters past the end of the file, so the
        // walk this handle left off at cannot be trusted any more.
        self.cur_cluster_relative = 0;
        self.cur_cluster_absolute = first_cluster;

        self.dirent.set_file_size(new_size as u32);
        Ok(())
    }

    /// Write the entry back, recording the file's size and the fact that it was
    /// just touched.
    fn commit(&mut self, fs: &mut FatxFs) -> Result<(), Error> {
        let location = self.location.ok_or(Error::NotFound)?;
        let now = DateTime::now();
        self.dirent.set_modified(&now, fs.variant);
        self.dirent.set_accessed(&now, fs.variant);
        self.dirent.write_at(fs, location)?;
        fs.flush_fat()
    }
}

impl io::Seek for File {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let target: i64 = match pos {
            io::SeekFrom::Start(offset) => offset as i64,
            io::SeekFrom::Current(offset) => (self.seek_pos as i64).saturating_add(offset),
            io::SeekFrom::End(offset) => self.dirent.file_size() as i64 + offset,
        };

        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Seek target cannot be negative",
            ));
        }

        // A file's size is a 32 bit field, so a position past that is one no
        // file could ever reach. Refusing it here keeps it from being truncated
        // into a valid position and writing over the start of the file.
        if target > u32::MAX as i64 {
            return Err(Error::FileTooLarge.into());
        }

        self.seek_pos = target as u32;
        Ok(self.seek_pos as u64)
    }
}

impl io::Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Get handle to filesystem interface
        let handle = self.handle.fs.clone();
        let mut fs = handle.lock().unwrap();

        let file_size: u32 = self.dirent.file_size();
        let mut buf_pos: u64 = 0;

        while buf_pos < buf.len() as u64 {
            // Ensure we don't read past EOF
            if self.seek_pos >= file_size {
                break;
            }
            let bytes_remaining_in_file: u32 = file_size - self.seek_pos;

            // Map current file position to cluster number
            let cluster = self.cluster_at_seek_pos(&mut fs, false)?;

            // Determine number of bytes to read in this cluster
            let byte_offset_in_cluster = self.seek_pos as u64 % fs.num_bytes_per_cluster;
            let bytes_remaining_in_cluster = fs.num_bytes_per_cluster - byte_offset_in_cluster;
            let bytes_remaining_in_dest = buf.len() as u64 - buf_pos;
            let bytes_to_read_from_cluster = min(
                min(bytes_remaining_in_dest, bytes_remaining_in_cluster),
                bytes_remaining_in_file as u64,
            );

            // Read cluster data from device
            log::debug!(
                "Reading {} bytes from cluster {}",
                bytes_to_read_from_cluster,
                cluster
            );
            fs.seek_cluster(cluster, byte_offset_in_cluster)?;
            let bytes_read = {
                let start = buf_pos as usize;
                let end = start + bytes_to_read_from_cluster as usize;
                io::Read::read(&mut fs.device_handle, &mut buf[start..end])?
            };

            // The entry can claim more bytes than the device actually holds, on
            // a truncated image or with a size given by hand. Stopping on a
            // read that returns nothing keeps that from spinning forever.
            if bytes_read == 0 {
                break;
            }

            // Advance
            buf_pos += bytes_read as u64;
            self.seek_pos += bytes_read as u32;
        }

        Ok(buf_pos as usize)
    }
}

impl io::Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Get handle to filesystem interface
        let handle = self.handle.fs.clone();
        let mut fs = handle.lock().unwrap();
        fs.ensure_writable()?;

        // A file needs a chain of its own before anything can go into it.
        let first_cluster = fs.ensure_first_cluster(&mut self.dirent)?;
        if self.cur_cluster_absolute == 0 {
            self.cur_cluster_absolute = first_cluster;
            self.cur_cluster_relative = 0;
        }

        // File sizes are a 32 bit field, so a write that would carry the
        // position past that is cut short rather than wrapping.
        let room = (u32::MAX - self.seek_pos) as usize;
        if room == 0 {
            return Err(Error::FileTooLarge.into());
        }
        let buf = &buf[..min(buf.len(), room)];

        // Writing past the end of the file leaves a gap, which FATX has no way
        // to express: it has to be backed by real clusters full of zeroes
        // before the new bytes go down after it.
        if self.seek_pos as u64 > self.dirent.file_size() as u64 {
            let target = self.seek_pos as u64;
            self.extend_to(&mut fs, target)?;
        }

        let mut buf_pos: usize = 0;
        let mut error = None;
        while buf_pos < buf.len() {
            // Map current file position to cluster number, extending the chain
            // when the write runs past the last cluster of the file.
            //
            // Running out of room here ends the write rather than failing it:
            // what has already gone down is on the device, and reporting a
            // failure would leave those bytes outside the file for good.
            let cluster = match self.cluster_at_seek_pos(&mut fs, true) {
                Ok(cluster) => cluster,
                Err(err) => {
                    error = Some(err);
                    break;
                }
            };

            // Determine number of bytes to write in this cluster
            let byte_offset_in_cluster = self.seek_pos as u64 % fs.num_bytes_per_cluster;
            let bytes_remaining_in_cluster = fs.num_bytes_per_cluster - byte_offset_in_cluster;
            let bytes_to_write = min(bytes_remaining_in_cluster, (buf.len() - buf_pos) as u64);

            log::debug!("Writing {bytes_to_write} bytes to cluster {cluster}");
            let bytes_written = match fs
                .seek_cluster(cluster, byte_offset_in_cluster)
                .map_err(io::Error::from)
                .and_then(|()| {
                    let start = buf_pos;
                    let end = start + bytes_to_write as usize;
                    io::Write::write(&mut fs.device_handle, &buf[start..end])
                }) {
                Ok(bytes_written) => bytes_written,
                Err(err) => {
                    error = Some(err.into());
                    break;
                }
            };
            if bytes_written == 0 {
                break;
            }

            // Advance
            buf_pos += bytes_written;
            self.seek_pos += bytes_written as u32;

            // The file has grown if the write went past its old end.
            if self.seek_pos > self.dirent.file_size() {
                self.dirent.set_file_size(self.seek_pos);
            }
        }

        // A short write is still a write: the caller is told how much of the
        // buffer went down, and only a write that placed nothing at all reports
        // what stopped it.
        if buf_pos == 0
            && let Some(err) = error
        {
            return Err(err.into());
        }
        self.commit(&mut fs)?;

        Ok(buf_pos)
    }

    fn flush(&mut self) -> io::Result<()> {
        let handle = self.handle.fs.clone();
        let mut fs = handle.lock().unwrap();
        fs.sync()?;
        Ok(())
    }
}
