use std::io::Write;
use std::path::{Component, Path};

use crate::variant::Variant;

use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::*;

pub const FATX_MAX_FILENAME_LEN: usize = 42;

// Markers used in the filename_size field of the directory entry.
const FATX_DELETED_FILE_MARKER: u8 = 0xe5;
const FATX_END_OF_DIR_MARKER: u8 = 0xff;
const FATX_END_OF_DIR_MARKER2: u8 = 0x00;

// Byte the unused tail of a filename is padded with, matching libfatx.
const FATX_FILENAME_PADDING: u8 = 0xff;

// Mask to be applied when reading directory entry attributes.
const FATX_ATTR_READ_ONLY: u8 = 1 << 0;
const FATX_ATTR_SYSTEM: u8 = 1 << 1;
const FATX_ATTR_HIDDEN: u8 = 1 << 2;
const FATX_ATTR_VOLUME: u8 = 1 << 3;
pub(crate) const FATX_ATTR_DIRECTORY: u8 = 1 << 4;

use crate::datetime::DateTime;
use crate::error::Error;
use crate::fat::{ClusterId, FatEntry};
use crate::fs::{FatxFs, FatxFsHandle};
use crate::path::normalize_virtual_path;

/// Where a directory entry lives on disk.
///
/// An entry has to be found again to be updated — its size after a write, its
/// name after a rename, its deletion marker after an unlink — and a directory
/// is a cluster chain rather than a flat array, so the cluster has to be
/// remembered alongside the index of the entry within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryLocation {
    pub(crate) cluster: ClusterId,
    pub(crate) index: u64,
}

impl EntryLocation {
    fn byte_offset_in_cluster(&self) -> u64 {
        self.index * std::mem::size_of::<DirectoryEntry>() as u64
    }
}

// The directory entry, as it appears on disk.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug, Clone)]
#[repr(C, packed)]
pub struct DirectoryEntry {
    filename_len: u8,
    attributes: u8,
    filename_bytes: [u8; FATX_MAX_FILENAME_LEN],
    first_cluster: U32,
    file_size: U32,
    modified_time: U16,
    modified_date: U16,
    created_time: U16,
    created_date: U16,
    accessed_time: U16,
    accessed_date: U16,
}

#[derive(Debug, PartialEq)]
pub enum DirectoryEntryKind {
    Valid,
    Deleted,
    EndOfDirectory,
}

impl DirectoryEntry {
    pub(crate) fn from_path<P: AsRef<Path>>(fs: &mut FatxFs, path: P) -> Result<Self, Error> {
        Ok(Self::lookup(fs, path)?.0)
    }

    /// Resolve a path to its directory entry and the place that entry occupies
    /// on disk.
    ///
    /// The root directory has no entry of its own — it is described by the
    /// superblock — so a synthetic entry is returned for it, with no location.
    pub(crate) fn lookup<P: AsRef<Path>>(
        fs: &mut FatxFs,
        path: P,
    ) -> Result<(Self, Option<EntryLocation>), Error> {
        let path = normalize_virtual_path(path);
        let num_components = path.components().count();

        if num_components == 0 {
            return Err(Error::NotADirectory);
        }

        // Step through path components to find target directory
        let mut cwd = None;
        for (comp_idx, comp) in path.components().enumerate() {
            cwd = match comp {
                Component::RootDir => Some(fs.root_cluster),
                Component::Normal(name) => {
                    if cwd.is_none() {
                        return Err(Error::NotFound);
                    }

                    // Scan directory entries to resolve this path component
                    let mut dir_iter = DirectoryEntryIterator::new(cwd.unwrap());
                    loop {
                        // Fetch next directory entry
                        let dirent_result = dir_iter.next(fs);
                        if dirent_result.is_none() {
                            return Err(Error::NotFound);
                        }

                        // Check current directory entry for target match
                        let dirent = dirent_result.unwrap()?;
                        if let DirectoryEntryKind::Valid = dirent.kind() {
                            if dirent.file_name() != name.to_str().unwrap() {
                                continue;
                            }
                            if comp_idx == (num_components - 1) {
                                let location = dir_iter.location();
                                return Ok((dirent, Some(location)));
                            }
                            if dirent.is_directory() {
                                break Some(dirent.first_cluster.into());
                            }
                            return Err(Error::NotADirectory);
                        }
                    }
                }
                _ => {
                    panic!("Unexpected path component!");
                }
            }
        }

        // Create a fake DirectoryEntry to represent the root directory
        Ok((
            Self {
                filename_len: 8,
                attributes: FATX_ATTR_DIRECTORY,
                filename_bytes: {
                    let mut filename_bytes = [0u8; FATX_MAX_FILENAME_LEN];
                    filename_bytes[0..4].copy_from_slice(b"root");
                    filename_bytes
                },
                first_cluster: fs.root_cluster.into(),
                file_size: 0.into(),
                modified_date: 0.into(),
                modified_time: 0.into(),
                created_time: 0.into(),
                created_date: 0.into(),
                accessed_time: 0.into(),
                accessed_date: 0.into(),
            },
            None,
        ))
    }

    /// Build a brand new entry for a file or directory.
    ///
    /// The unused tail of the filename is padded rather than zeroed, so that
    /// the bytes written match what the consoles and libfatx write.
    pub(crate) fn new_node(
        name: &str,
        attributes: u8,
        first_cluster: ClusterId,
        timestamp: &DateTime,
        variant: Variant,
    ) -> Result<Self, Error> {
        let mut entry = Self {
            filename_len: 0,
            attributes,
            filename_bytes: [FATX_FILENAME_PADDING; FATX_MAX_FILENAME_LEN],
            first_cluster: first_cluster.into(),
            file_size: 0.into(),
            modified_time: 0.into(),
            modified_date: 0.into(),
            created_time: 0.into(),
            created_date: 0.into(),
            accessed_time: 0.into(),
            accessed_date: 0.into(),
        };
        entry.set_file_name(name)?;
        entry.set_created(timestamp, variant);
        entry.set_modified(timestamp, variant);
        entry.set_accessed(timestamp, variant);
        Ok(entry)
    }

    pub(crate) fn kind(&self) -> DirectoryEntryKind {
        match self.filename_len {
            FATX_END_OF_DIR_MARKER => DirectoryEntryKind::EndOfDirectory,
            FATX_END_OF_DIR_MARKER2 => DirectoryEntryKind::EndOfDirectory,
            FATX_DELETED_FILE_MARKER => DirectoryEntryKind::Deleted,
            _ => DirectoryEntryKind::Valid,
        }
    }

    pub fn file_name(&self) -> String {
        assert_eq!(self.kind(), DirectoryEntryKind::Valid);
        let bytes = &self.filename_bytes[..self.filename_len as usize];
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Rename this entry.
    ///
    /// A name has to be non-empty, short enough to fit the fixed field, and
    /// must not collide with either of the markers that the length byte doubles
    /// as: a 42 byte name is representable but 0xe5 and 0xff are not lengths.
    pub(crate) fn set_file_name(&mut self, name: &str) -> Result<(), Error> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > FATX_MAX_FILENAME_LEN {
            return Err(Error::InvalidFileName);
        }
        if name.contains('/') || name.contains('\0') {
            return Err(Error::InvalidFileName);
        }

        self.filename_len = bytes.len() as u8;
        self.filename_bytes = [FATX_FILENAME_PADDING; FATX_MAX_FILENAME_LEN];
        self.filename_bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    pub fn file_size(&self) -> u32 {
        self.file_size.into()
    }

    pub(crate) fn set_file_size(&mut self, size: u32) {
        self.file_size = size.into();
    }

    pub fn is_directory(&self) -> bool {
        !self.is_volume() && self.attributes & FATX_ATTR_DIRECTORY == FATX_ATTR_DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        !self.is_volume() && !self.is_directory()
    }

    pub fn is_hidden(&self) -> bool {
        self.attributes & FATX_ATTR_HIDDEN == FATX_ATTR_HIDDEN
    }

    pub fn is_read_only(&self) -> bool {
        self.attributes & FATX_ATTR_READ_ONLY == FATX_ATTR_READ_ONLY
    }

    pub fn is_volume(&self) -> bool {
        self.attributes & FATX_ATTR_VOLUME == FATX_ATTR_VOLUME
    }

    pub fn is_system(&self) -> bool {
        self.attributes & FATX_ATTR_SYSTEM == FATX_ATTR_SYSTEM
    }

    pub fn created(&self, variant: Variant) -> DateTime {
        DateTime::from_fatx_encoding(self.created_date.into(), self.created_time.into(), variant)
    }

    pub fn modified(&self, variant: Variant) -> DateTime {
        DateTime::from_fatx_encoding(
            self.modified_date.into(),
            self.modified_time.into(),
            variant,
        )
    }

    pub fn accessed(&self, variant: Variant) -> DateTime {
        DateTime::from_fatx_encoding(
            self.accessed_date.into(),
            self.accessed_time.into(),
            variant,
        )
    }

    /// Timestamps are held packed, in the encoding of the filesystem they came
    /// from or are going to: the two variants disagree on the epoch and on the
    /// width of the hour and minute fields, so a packed timestamp only means
    /// anything alongside its variant. Only the byte order and the order of the
    /// two halves are left to `denormalize` on the way out to disk.
    pub(crate) fn set_created(&mut self, timestamp: &DateTime, variant: Variant) {
        let (date, time) = timestamp.to_fatx_encoding(variant);
        self.created_date = date.into();
        self.created_time = time.into();
    }

    pub(crate) fn set_modified(&mut self, timestamp: &DateTime, variant: Variant) {
        let (date, time) = timestamp.to_fatx_encoding(variant);
        self.modified_date = date.into();
        self.modified_time = time.into();
    }

    pub(crate) fn set_accessed(&mut self, timestamp: &DateTime, variant: Variant) {
        let (date, time) = timestamp.to_fatx_encoding(variant);
        self.accessed_date = date.into();
        self.accessed_time = time.into();
    }

    pub(crate) fn first_cluster(&self) -> ClusterId {
        self.first_cluster.into()
    }

    pub(crate) fn set_first_cluster(&mut self, cluster: ClusterId) {
        self.first_cluster = cluster.into();
    }

    /// Convert this entry from on-disk form into the canonical little-endian,
    /// time-then-date layout the accessors above expect.
    ///
    /// Must be called on every entry read from a device. For the original Xbox
    /// it does nothing. For the 360 it swaps each multi-byte field, and swaps
    /// the two halves of each timestamp: the 360 stores the date first, so the
    /// field named modified_time holds the date on that console.
    pub(crate) fn normalize(&mut self, variant: Variant) {
        if !variant.needs_swap() {
            return;
        }

        self.first_cluster = u32::from(self.first_cluster).swap_bytes().into();
        self.file_size = u32::from(self.file_size).swap_bytes().into();

        // Each pair arrives as (date, time) and must end up as (time, date),
        // with both halves byte-swapped.
        fn swap_pair(slot0: &mut U16, slot1: &mut U16) {
            let date = u16::from(*slot0).swap_bytes();
            let time = u16::from(*slot1).swap_bytes();
            *slot0 = time.into();
            *slot1 = date.into();
        }

        swap_pair(&mut self.modified_time, &mut self.modified_date);
        swap_pair(&mut self.created_time, &mut self.created_date);
        swap_pair(&mut self.accessed_time, &mut self.accessed_date);
    }

    /// Convert this entry from the canonical form back into on-disk form.
    ///
    /// Swapping a pair of fields and swapping their bytes are both their own
    /// inverse, so this is the same operation as `normalize`; it exists under
    /// its own name so that call sites say which way they are converting.
    pub(crate) fn denormalize(&mut self, variant: Variant) {
        self.normalize(variant);
    }

    /// Write this entry over the one at `location`.
    pub(crate) fn write_at(&self, fs: &mut FatxFs, location: EntryLocation) -> Result<(), Error> {
        let mut raw = self.clone();
        raw.denormalize(fs.variant);

        fs.seek_cluster(location.cluster, location.byte_offset_in_cluster())?;
        fs.device_handle.write_all(raw.as_bytes())?;
        Ok(())
    }
}

/// Overwrite just the length byte of an entry, turning it into a marker.
///
/// Only that one byte distinguishes a live entry from a deleted one or from
/// the end of the directory, so the rest of the entry is left untouched.
pub(crate) fn set_entry_marker(
    fs: &mut FatxFs,
    location: EntryLocation,
    marker: u8,
) -> Result<(), Error> {
    fs.seek_cluster(location.cluster, location.byte_offset_in_cluster())?;
    fs.device_handle.write_all(&[marker])?;
    Ok(())
}

pub(crate) fn mark_entry_deleted(fs: &mut FatxFs, location: EntryLocation) -> Result<(), Error> {
    set_entry_marker(fs, location, FATX_DELETED_FILE_MARKER)
}

pub(crate) fn mark_end_of_directory(fs: &mut FatxFs, location: EntryLocation) -> Result<(), Error> {
    set_entry_marker(fs, location, FATX_END_OF_DIR_MARKER)
}

/// Find a place in a directory for a new entry.
///
/// A deleted entry is reused if there is one. Otherwise the entry that marks
/// the end of the directory is taken over, and the marker is pushed one slot
/// along — into a freshly allocated cluster if the directory has run out of
/// room in this one.
pub(crate) fn alloc_entry(fs: &mut FatxFs, dir_cluster: ClusterId) -> Result<EntryLocation, Error> {
    let entries_per_cluster = fs.num_entries_per_cluster;
    let mut iter = DirectoryEntryIterator::new(dir_cluster);

    while let Some(entry) = iter.next(fs) {
        let entry = entry?;
        let location = iter.location();
        match entry.kind() {
            DirectoryEntryKind::Deleted => return Ok(location),
            DirectoryEntryKind::EndOfDirectory => {
                // Take this slot and push the end marker one along.
                if location.index + 1 < entries_per_cluster {
                    mark_end_of_directory(
                        fs,
                        EntryLocation {
                            cluster: location.cluster,
                            index: location.index + 1,
                        },
                    )?;
                } else {
                    let next = next_directory_cluster(fs, location.cluster)?;
                    mark_end_of_directory(
                        fs,
                        EntryLocation {
                            cluster: next,
                            index: 0,
                        },
                    )?;
                }
                return Ok(location);
            }
            DirectoryEntryKind::Valid => continue,
        }
    }

    // Every slot of every cluster of this directory is occupied and the chain
    // ended without an end-of-directory marker. Carry on into a new cluster.
    let cluster = fs.alloc_cluster(true)?;
    fs.attach_cluster(iter.cluster, cluster)?;
    mark_end_of_directory(fs, EntryLocation { cluster, index: 1 })?;
    Ok(EntryLocation { cluster, index: 0 })
}

/// The cluster following `cluster` in a directory chain, extending the chain if
/// it ends there.
fn next_directory_cluster(fs: &mut FatxFs, cluster: ClusterId) -> Result<ClusterId, Error> {
    if let Some(next) = fs.next_cluster(cluster)? {
        return Ok(next);
    }

    let next = fs.alloc_cluster(true)?;
    fs.attach_cluster(cluster, next)?;
    Ok(next)
}

/// Whether a directory holds no live entries.
pub(crate) fn directory_is_empty(fs: &mut FatxFs, dir_cluster: ClusterId) -> Result<bool, Error> {
    let mut iter = DirectoryEntryIterator::new(dir_cluster);
    while let Some(entry) = iter.next(fs) {
        match entry?.kind() {
            DirectoryEntryKind::Valid => return Ok(false),
            DirectoryEntryKind::Deleted => continue,
            DirectoryEntryKind::EndOfDirectory => break,
        }
    }
    Ok(true)
}

pub(crate) struct DirectoryEntryIterator {
    cluster: ClusterId,
    entry: i64,
    finished: bool,
}

impl DirectoryEntryIterator {
    pub(crate) fn new(cluster: ClusterId) -> Self {
        Self {
            cluster,
            entry: -1,
            finished: false,
        }
    }

    /// Where the entry just returned by `next` lives on disk.
    pub(crate) fn location(&self) -> EntryLocation {
        debug_assert!(self.entry >= 0, "no entry has been read yet");
        EntryLocation {
            cluster: self.cluster,
            index: self.entry as u64,
        }
    }

    pub(crate) fn next(&mut self, fs: &mut FatxFs) -> Option<Result<DirectoryEntry, Error>> {
        if self.finished {
            return None;
        }

        self.entry += 1;

        if (self.entry >= 0) && (self.entry as u64 >= fs.num_entries_per_cluster) {
            // Advance to next cluster
            let fat_entry = fs.fat.entry(self.cluster, fs.variant);
            match fat_entry {
                Err(err) => {
                    self.finished = true;
                    return Some(Err(err));
                }
                Ok(FatEntry::Data(next_cluster)) => {
                    self.cluster = next_cluster as ClusterId;
                    self.entry = 0;
                }
                Ok(FatEntry::End) => {
                    // The directory has no more clusters. Callers looking to
                    // extend it need to know where the chain ended, which is
                    // the cluster this iterator is still sitting on.
                    self.finished = true;
                    return None;
                }
                _ => {
                    self.finished = true;
                    return Some(Err(Error::InvalidClusterChain));
                }
            }
        }

        // Fetch next entry
        if let Err(seek_error) = fs.seek_cluster(
            self.cluster,
            self.entry as u64 * std::mem::size_of::<DirectoryEntry>() as u64,
        ) {
            return Some(Err(seek_error));
        }

        match DirectoryEntry::read_from_io(&mut fs.device_handle) {
            Err(err) => {
                self.finished = true;
                Some(Err(err.into()))
            }
            Ok(mut entry) => {
                entry.normalize(fs.variant);
                if entry.kind() == DirectoryEntryKind::EndOfDirectory {
                    self.finished = true;
                }
                Some(Ok(entry))
            }
        }
    }
}

pub struct DirectoryEntryIntoIterator {
    pub fs: FatxFsHandle,
    entry_iter: DirectoryEntryIterator,
}

impl DirectoryEntryIntoIterator {
    pub fn new(handle: FatxFsHandle, cluster: ClusterId) -> Self {
        Self {
            fs: handle,
            entry_iter: DirectoryEntryIterator::new(cluster),
        }
    }
}

impl Iterator for DirectoryEntryIntoIterator {
    type Item = Result<DirectoryEntry, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.fs.with_lock(|fs| {
            loop {
                // Filter-in only valid directory entry kinds
                let item = self.entry_iter.next(fs);
                if let Some(Ok(dirent)) = item {
                    if let DirectoryEntryKind::Valid = dirent.kind() {
                        return Some(Ok(dirent));
                    } else {
                        continue;
                    }
                }
                return item;
            }
        })
    }
}
