use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use crate::datetime::DateTime;
use crate::dir::{
    self, DirectoryEntry, DirectoryEntryIntoIterator, EntryLocation, FATX_ATTR_DIRECTORY,
};
use crate::error::Error;
use crate::fat::{ClusterId, Fat, FatEntry};
use crate::file::File;
use crate::partition::{DEFAULT_PARTITION_LAYOUT, PartitionMapEntry};
use crate::path::normalize_virtual_path;
use crate::variant::Variant;

use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::*;

const FATX_SIGNATURE: u32 = 0x58544146; // 'FATX'
const FATX_FAT_OFFSET_BYTES: u64 = 4096;
const FATX_FAT_RESERVED_ENTRIES_COUNT: u32 = 1;

/// The first cluster a search for free space may return.
///
/// Entry 0 holds the media descriptor and entry 1 is the root directory, so
/// neither is ever handed out.
const FATX_FIRST_ALLOCATABLE_CLUSTER: ClusterId = 2;

// The superblock, as it appears on disk.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C, packed)]
pub(crate) struct Superblock {
    signature: U32,
    volume_id: U32,
    num_sectors_per_cluster: U32,
    root_cluster: U32,
    unknown_0: U16,
    padding: [u8; 4078],
}

impl Superblock {
    /// Convert from on-disk form into canonical little-endian.
    ///
    /// Does nothing for the original Xbox; swaps every field for the 360. The
    /// signature is deliberately left alone: it is byte-identical in both
    /// flavours and has already been used to work out which one this is.
    fn normalize(&mut self, variant: Variant) {
        if !variant.needs_swap() {
            return;
        }

        self.volume_id = u32::from(self.volume_id).swap_bytes().into();
        self.num_sectors_per_cluster = u32::from(self.num_sectors_per_cluster).swap_bytes().into();
        self.root_cluster = u32::from(self.root_cluster).swap_bytes().into();
        self.unknown_0 = u16::from(self.unknown_0).swap_bytes().into();
    }
}

#[derive(Debug)]
pub struct FatxFs {
    self_handle: Weak<Mutex<FatxFs>>,

    pub(crate) device_handle: std::fs::File,
    pub(crate) variant: Variant,
    pub(crate) writable: bool,
    pub(crate) partition_offset_bytes: u64,
    pub(crate) partition_size_bytes: u64,
    pub(crate) num_clusters: u32,
    pub(crate) num_bytes_per_cluster: u64,
    pub(crate) num_entries_per_cluster: u64,
    pub(crate) root_cluster: u32,
    pub(crate) fat_offset_bytes: u64,
    pub(crate) cluster_offset_bytes: u64,
    pub(crate) fat: Fat,
    /// Where the next search for a free cluster starts.
    ///
    /// Allocation walks the FAT from here and wraps, so filling a filesystem
    /// does not rescan the same occupied entries over and over.
    alloc_hint: ClusterId,
}

pub struct FatxFsConfig {
    device_path: String,
    partition_offset_bytes: u64,
    partition_size_bytes: u64,
    num_bytes_per_sector: u64,
    variant: Variant,
    writable: bool,
}

impl FatxFsConfig {
    pub fn new(device_path: String) -> Self {
        let partition = &DEFAULT_PARTITION_LAYOUT[3];
        Self {
            device_path,
            partition_offset_bytes: partition.offset_bytes,
            partition_size_bytes: partition.size_bytes,
            num_bytes_per_sector: 512,
            variant: Variant::Auto,
            writable: false,
        }
    }

    pub fn drive_letter(mut self, letter: &str) -> Self {
        let partition_info =
            PartitionMapEntry::from_letter(letter).expect("invalid partition letter");
        self.partition_offset_bytes = partition_info.offset_bytes;
        self.partition_size_bytes = partition_info.size_bytes;
        self
    }

    /// Select an Xbox 360 partition by name.
    pub fn x360_partition(mut self, name: &str) -> Self {
        let partition_info =
            PartitionMapEntry::from_x360_name(name).expect("invalid partition name");
        self.partition_offset_bytes = partition_info.offset_bytes;
        self.partition_size_bytes = partition_info.size_bytes;
        self
    }

    /// Force the on-disk byte order rather than detecting it.
    pub fn variant(mut self, variant: Variant) -> Self {
        self.variant = variant;
        self
    }

    /// Open the device for writing as well as reading.
    ///
    /// Off by default: a filesystem opened read-only cannot be damaged by a
    /// mistake in the caller, and the device itself is opened without write
    /// access, so the kernel refuses writes even if the library were asked for
    /// one.
    pub fn writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }

    pub fn partition_offset_bytes(mut self, offset: u64) -> Self {
        self.partition_offset_bytes = offset;
        self
    }

    pub fn partition_size_bytes(mut self, size: u64) -> Self {
        self.partition_size_bytes = size;
        self
    }
}

impl FatxFs {
    pub fn open_device(config: &FatxFsConfig) -> Result<FatxFsHandle, Error> {
        // Partition offset and size validation
        if !config
            .partition_offset_bytes
            .is_multiple_of(config.num_bytes_per_sector)
        {
            return Err(Error::InvalidPartitionOffset);
        }
        // Open device
        let mut device_handle = std::fs::OpenOptions::new()
            .read(true)
            .write(config.writable)
            .open(&config.device_path)?;

        // A size of u64::MAX means "the rest of the device", which is how the
        // partitions that run to the end of the disk are described. Resolve it
        // against the device's actual length, rounded down to a whole sector.
        let partition_size_bytes = if config.partition_size_bytes == u64::MAX {
            let device_size = device_handle.seek(SeekFrom::End(0))?;
            if device_size <= config.partition_offset_bytes {
                return Err(Error::InvalidPartitionSize);
            }
            let remaining = device_size - config.partition_offset_bytes;
            remaining - (remaining % config.num_bytes_per_sector)
        } else {
            config.partition_size_bytes
        };

        if !partition_size_bytes.is_multiple_of(config.num_bytes_per_sector) {
            return Err(Error::InvalidPartitionSize);
        }

        device_handle.seek(SeekFrom::Start(config.partition_offset_bytes))?;

        // Read superblock. The raw signature identifies the on-disk byte order,
        // so checking it and resolving the variant are the same step.
        let mut superblock = Superblock::read_from_io(&mut device_handle)?;
        let detected = Variant::from_raw_signature(superblock.signature.into(), FATX_SIGNATURE)
            .ok_or(Error::InvalidFilesystemSignature)?;
        let variant = match config.variant {
            Variant::Auto => {
                log::info!("Detected {detected:?} filesystem");
                detected
            }
            requested if requested == detected => requested,
            requested => {
                log::error!("Filesystem is {detected:?}, but {requested:?} was requested");
                return Err(Error::InvalidFilesystemSignature);
            }
        };
        superblock.normalize(variant);

        // Cluster geometry
        let num_sectors_per_cluster: u64 = superblock.num_sectors_per_cluster.into();
        if !(num_sectors_per_cluster.is_power_of_two() && num_sectors_per_cluster <= 1024) {
            return Err(Error::InvalidSectorsPerCluster);
        }
        let num_bytes_per_cluster = num_sectors_per_cluster * config.num_bytes_per_sector;
        let num_entries_per_cluster: u64 =
            num_bytes_per_cluster / (std::mem::size_of::<DirectoryEntry>() as u64);
        let root_cluster: u32 = superblock.root_cluster.into();

        // Calculate FAT size
        let fat_offset_bytes = config.partition_offset_bytes + FATX_FAT_OFFSET_BYTES;
        let num_fat_entries = (partition_size_bytes / num_bytes_per_cluster) as u32;
        if root_cluster >= num_fat_entries {
            log::error!("Root cluster of {} exceeds cluster limit", root_cluster);
            return Err(Error::InvalidRootCluster);
        }

        // FIXME: Make FAT management smarter
        let mut fat = Fat::new(num_fat_entries);
        device_handle.seek(SeekFrom::Start(fat_offset_bytes))?;
        device_handle.read_exact(&mut fat.fat_data)?;

        // Cluster geometry cont'd
        let cluster_offset_bytes = fat_offset_bytes + fat.fat_size_bytes;
        let num_clusters = ((partition_size_bytes - fat.fat_size_bytes - FATX_FAT_OFFSET_BYTES)
            / num_bytes_per_cluster
            + FATX_FAT_RESERVED_ENTRIES_COUNT as u64) as u32;

        let writable = config.writable;
        let fs = Arc::new_cyclic(move |weak_self| {
            Mutex::new(FatxFs {
                self_handle: weak_self.clone(),
                device_handle,
                variant,
                writable,
                partition_offset_bytes: config.partition_offset_bytes,
                partition_size_bytes,
                num_clusters,
                num_bytes_per_cluster,
                num_entries_per_cluster,
                root_cluster,
                fat_offset_bytes,
                cluster_offset_bytes,
                fat,
                alloc_hint: FATX_FIRST_ALLOCATABLE_CLUSTER,
            })
        });

        Ok(FatxFsHandle { fs })
    }

    fn handle(&self) -> FatxFsHandle {
        FatxFsHandle {
            fs: self.self_handle.upgrade().unwrap().clone(),
        }
    }

    pub(crate) fn cluster_to_byte_offset(&self, cluster: ClusterId) -> Result<u64, Error> {
        if cluster >= self.num_clusters + FATX_FAT_RESERVED_ENTRIES_COUNT {
            return Err(Error::InvalidClusterNumber);
        }

        let byte_offset: u64 = self.cluster_offset_bytes
            + (cluster - FATX_FAT_RESERVED_ENTRIES_COUNT) as u64 * self.num_bytes_per_cluster;
        debug_assert!(byte_offset < (self.partition_offset_bytes + self.partition_size_bytes));

        Ok(byte_offset)
    }

    pub(crate) fn seek_cluster(
        &mut self,
        cluster: ClusterId,
        offset_in_cluster: u64,
    ) -> Result<(), Error> {
        self.device_handle.seek(SeekFrom::Start(
            self.cluster_to_byte_offset(cluster)? + offset_in_cluster,
        ))?;
        Ok(())
    }

    pub(crate) fn stat<P: AsRef<Path>>(&mut self, path: P) -> Result<DirectoryEntry, Error> {
        DirectoryEntry::from_path(self, path)
    }

    pub(crate) fn open<P: AsRef<Path>>(&mut self, path: P) -> Result<File, Error> {
        let (dirent, location) = DirectoryEntry::lookup(self, path)?;
        if dirent.is_file() {
            Ok(File::new(self.handle(), dirent, location))
        } else {
            Err(Error::IsADirectory)
        }
    }

    pub(crate) fn read_dir(&mut self, path: &str) -> Result<DirectoryEntryIntoIterator, Error> {
        let dirent = DirectoryEntry::from_path(self, path)?;
        if !dirent.is_directory() {
            return Err(Error::NotADirectory);
        }
        Ok(DirectoryEntryIntoIterator::new(
            self.handle(),
            dirent.first_cluster(),
        ))
    }

    // -- Writing ------------------------------------------------------------

    /// Refuse the operation unless the filesystem was opened for writing.
    pub(crate) fn ensure_writable(&self) -> Result<(), Error> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::ReadOnlyFilesystem)
        }
    }

    /// The cluster following `cluster`, or None if the chain ends there.
    pub(crate) fn next_cluster(&mut self, cluster: ClusterId) -> Result<Option<ClusterId>, Error> {
        match self.fat.entry(cluster, self.variant)? {
            FatEntry::Data(next) => Ok(Some(next)),
            FatEntry::End => Ok(None),
            _ => Err(Error::InvalidClusterChain),
        }
    }

    /// Claim a free cluster and mark it as the end of a chain.
    ///
    /// Zeroing is optional because a caller about to overwrite the whole
    /// cluster with file data would only be writing it twice; anything holding
    /// structure — a directory, or a cluster that will be partly written — must
    /// ask for it, or stale bytes from a deleted file show through.
    pub(crate) fn alloc_cluster(&mut self, zero: bool) -> Result<ClusterId, Error> {
        self.ensure_writable()?;

        let last = self.num_clusters;
        let mut cluster = None;

        // Walk the FAT from the hint, wrapping once, so that every allocatable
        // entry is considered exactly once before giving up.
        let mut index = self.alloc_hint;
        for _ in FATX_FIRST_ALLOCATABLE_CLUSTER..last {
            if index >= last {
                index = FATX_FIRST_ALLOCATABLE_CLUSTER;
            }
            if self.fat.entry(index, self.variant)? == FatEntry::Available {
                cluster = Some(index);
                break;
            }
            index += 1;
        }

        let cluster = cluster.ok_or(Error::NoSpaceLeft)?;
        self.alloc_hint = cluster + 1;
        self.fat.set_entry(cluster, FatEntry::End, self.variant)?;

        if zero {
            self.zero_cluster(cluster)?;
        }

        log::debug!("Allocated cluster {cluster}");
        Ok(cluster)
    }

    /// Overwrite a whole cluster with zeroes.
    pub(crate) fn zero_cluster(&mut self, cluster: ClusterId) -> Result<(), Error> {
        self.zero_range(cluster, 0, self.num_bytes_per_cluster)
    }

    /// Overwrite part of a cluster with zeroes.
    pub(crate) fn zero_range(
        &mut self,
        cluster: ClusterId,
        offset_in_cluster: u64,
        len: u64,
    ) -> Result<(), Error> {
        if len == 0 {
            return Ok(());
        }

        self.seek_cluster(cluster, offset_in_cluster)?;
        let zeroes = vec![0u8; len as usize];
        self.device_handle.write_all(&zeroes)?;
        Ok(())
    }

    /// Append a cluster to the chain ending at `tail`.
    pub(crate) fn attach_cluster(
        &mut self,
        tail: ClusterId,
        cluster: ClusterId,
    ) -> Result<(), Error> {
        self.ensure_writable()?;

        if self.fat.entry(tail, self.variant)? != FatEntry::End {
            log::error!("Cluster {tail} is not the last of its chain");
            return Err(Error::InvalidClusterChain);
        }

        self.fat
            .set_entry(tail, FatEntry::Data(cluster), self.variant)?;
        self.fat.set_entry(cluster, FatEntry::End, self.variant)?;
        Ok(())
    }

    /// Mark every cluster of a chain as free.
    pub(crate) fn free_cluster_chain(&mut self, first_cluster: ClusterId) -> Result<(), Error> {
        self.ensure_writable()?;

        let mut cluster = first_cluster;
        loop {
            // Read the link before dropping it, or the rest of the chain is
            // unreachable.
            let next = self.next_cluster(cluster)?;
            self.fat
                .set_entry(cluster, FatEntry::Available, self.variant)?;

            // A freed cluster is a good place for the next allocation to look.
            self.alloc_hint = self.alloc_hint.min(cluster);

            match next {
                Some(next) => cluster = next,
                None => break,
            }
        }

        Ok(())
    }

    /// Write out everything held in memory and ask the device to persist it.
    pub(crate) fn sync(&mut self) -> Result<(), Error> {
        self.flush_fat()?;
        if self.writable {
            self.device_handle.sync_all()?;
        }
        Ok(())
    }

    /// Write the modified part of the FAT back to the device.
    ///
    /// Called at the end of every operation that changes the FAT, so that the
    /// allocation state on disk never lags behind the data written against it.
    pub(crate) fn flush_fat(&mut self) -> Result<(), Error> {
        let fat_offset_bytes = self.fat_offset_bytes;
        // Two disjoint fields of self, which the borrow checker cannot see
        // through a method call.
        let Self {
            fat, device_handle, ..
        } = self;
        fat.flush(device_handle, fat_offset_bytes)
    }

    /// Resolve the parent directory of a path, and the name within it.
    fn split_parent<P: AsRef<Path>>(&mut self, path: P) -> Result<(ClusterId, String), Error> {
        let path = normalize_virtual_path(path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::IsRootDirectory)?
            .to_string();
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();

        let parent_dirent = DirectoryEntry::from_path(self, &parent)?;
        if !parent_dirent.is_directory() {
            return Err(Error::NotADirectory);
        }

        Ok((parent_dirent.first_cluster(), name))
    }

    /// Create a file or directory and return its entry.
    fn create_node<P: AsRef<Path>>(
        &mut self,
        path: P,
        attributes: u8,
    ) -> Result<(DirectoryEntry, EntryLocation), Error> {
        self.ensure_writable()?;

        let path = normalize_virtual_path(path);
        match DirectoryEntry::lookup(self, &path) {
            Ok(_) => return Err(Error::AlreadyExists),
            Err(Error::NotFound) => {}
            Err(err) => return Err(err),
        }

        let (parent_cluster, name) = self.split_parent(&path)?;

        // Every node owns a cluster from the moment it is created, as the
        // consoles' own filesystem does: a directory needs one to hold its end
        // marker, and giving files one too keeps the chain walk uniform.
        let cluster = self.alloc_cluster(true)?;

        let result = self.link_node(parent_cluster, &name, attributes, cluster);
        if result.is_err() {
            // Give the cluster back rather than leaking it.
            let _ = self.free_cluster_chain(cluster);
        }

        self.flush_fat()?;
        result
    }

    /// Put a new entry for an already allocated chain into a directory.
    fn link_node(
        &mut self,
        parent_cluster: ClusterId,
        name: &str,
        attributes: u8,
        cluster: ClusterId,
    ) -> Result<(DirectoryEntry, EntryLocation), Error> {
        let location = dir::alloc_entry(self, parent_cluster)?;
        let entry =
            DirectoryEntry::new_node(name, attributes, cluster, &DateTime::now(), self.variant)?;
        entry.write_at(self, location)?;
        Ok((entry, location))
    }

    /// Create an empty file and open it for writing.
    fn create<P: AsRef<Path>>(&mut self, path: P) -> Result<File, Error> {
        let (dirent, location) = self.create_node(path, 0)?;
        Ok(File::new(self.handle(), dirent, Some(location)))
    }

    /// Create a directory.
    fn mkdir<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        let (dirent, _) = self.create_node(path, FATX_ATTR_DIRECTORY)?;

        // The cluster was allocated zeroed, which already reads as the end of
        // the directory, but the marker is written explicitly so that the
        // bytes on disk match what the consoles write.
        dir::mark_end_of_directory(
            self,
            EntryLocation {
                cluster: dirent.first_cluster(),
                index: 0,
            },
        )
    }

    /// Remove a file.
    fn unlink<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        self.ensure_writable()?;

        let (dirent, location) = DirectoryEntry::lookup(self, path)?;
        let location = location.ok_or(Error::IsRootDirectory)?;
        if dirent.is_directory() {
            return Err(Error::IsADirectory);
        }

        // An empty file written by another tool may own no cluster at all,
        // and there would be no chain to give back.
        if dirent.first_cluster() != 0 {
            self.free_cluster_chain(dirent.first_cluster())?;
        }
        dir::mark_entry_deleted(self, location)?;
        self.flush_fat()
    }

    /// Remove an empty directory.
    fn rmdir<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        self.ensure_writable()?;

        let (dirent, location) = DirectoryEntry::lookup(self, path)?;
        let location = location.ok_or(Error::IsRootDirectory)?;
        if !dirent.is_directory() {
            return Err(Error::NotADirectory);
        }
        if !dir::directory_is_empty(self, dirent.first_cluster())? {
            return Err(Error::DirectoryNotEmpty);
        }

        if dirent.first_cluster() != 0 {
            self.free_cluster_chain(dirent.first_cluster())?;
        }
        dir::mark_entry_deleted(self, location)?;
        self.flush_fat()
    }

    /// Grow or shrink a file, and update its entry.
    fn truncate<P: AsRef<Path>>(&mut self, path: P, size: u64) -> Result<(), Error> {
        self.ensure_writable()?;

        let (mut dirent, location) = DirectoryEntry::lookup(self, path)?;
        let location = location.ok_or(Error::IsADirectory)?;
        if dirent.is_directory() {
            return Err(Error::IsADirectory);
        }
        if size > u32::MAX as u64 {
            return Err(Error::FileTooLarge);
        }

        let old_size = dirent.file_size() as u64;
        let first_cluster = self.ensure_first_cluster(&mut dirent)?;

        // A file keeps at least its first cluster, so that its entry always
        // points at a real chain.
        let wanted = size.div_ceil(self.num_bytes_per_cluster).max(1);

        // Walk the chain, extending it if it is short of what is wanted.
        let mut cluster = first_cluster;
        for _ in 1..wanted {
            cluster = match self.next_cluster(cluster)? {
                Some(next) => next,
                None => {
                    let next = self.alloc_cluster(true)?;
                    self.attach_cluster(cluster, next)?;
                    next
                }
            };
        }

        // Anything past that is no longer part of the file.
        if let Some(next) = self.next_cluster(cluster)? {
            self.free_cluster_chain(next)?;
            self.fat.set_entry(cluster, FatEntry::End, self.variant)?;
        }

        // Bytes between the old end of the file and the new one must read as
        // zero.
        // Clusters added above were allocated zeroed, and so was any cluster
        // reused from the free pool, so only the cluster the file used to end
        // in can still be holding bytes that are now part of it.
        if size > old_size {
            let tail_cluster = self.cluster_for_offset(first_cluster, old_size, false)?;
            let tail_offset = old_size % self.num_bytes_per_cluster;
            let len = (self.num_bytes_per_cluster - tail_offset).min(size - old_size);
            self.zero_range(tail_cluster, tail_offset, len)?;
        }

        dirent.set_file_size(size as u32);
        dirent.set_modified(&DateTime::now(), self.variant);
        dirent.write_at(self, location)?;
        self.flush_fat()
    }

    /// Move or rename a file or directory.
    ///
    /// The entry is copied to its new home and the old one marked deleted; the
    /// cluster chain itself is untouched, so the contents never move.
    fn rename<P: AsRef<Path>, Q: AsRef<Path>>(&mut self, from: P, to: Q) -> Result<(), Error> {
        self.ensure_writable()?;

        let from = normalize_virtual_path(from);
        let to = normalize_virtual_path(to);
        if from == to {
            return Ok(());
        }

        let (mut dirent, from_location) = DirectoryEntry::lookup(self, &from)?;
        let from_location = from_location.ok_or(Error::IsRootDirectory)?;

        // Moving a directory into itself would detach it from the tree.
        if dirent.is_directory() && to.starts_with(&from) {
            return Err(Error::InvalidRename);
        }

        let (to_parent_cluster, to_name) = self.split_parent(&to)?;
        dirent.set_file_name(&to_name)?;

        // Claim a slot for the new entry before touching anything else. The
        // claim is what can run the filesystem out of space, and doing it
        // first means a full disk leaves both ends of the rename intact rather
        // than taking the destination away and then failing.
        let to_location = dir::alloc_entry(self, to_parent_cluster)?;

        // Replace whatever is already at the destination, as rename(2) does.
        match DirectoryEntry::lookup(self, &to) {
            Ok((existing, _)) => {
                if existing.is_directory() {
                    self.rmdir(&to)?;
                } else {
                    self.unlink(&to)?;
                }
            }
            Err(Error::NotFound) => {}
            Err(err) => return Err(err),
        }

        dirent.write_at(self, to_location)?;

        // Only the entry is removed: the chain now belongs to the new entry.
        dir::mark_entry_deleted(self, from_location)?;
        self.flush_fat()
    }

    /// Overwrite the timestamps of an existing node.
    fn set_times<P: AsRef<Path>>(
        &mut self,
        path: P,
        accessed: Option<DateTime>,
        modified: Option<DateTime>,
    ) -> Result<(), Error> {
        self.ensure_writable()?;

        let (mut dirent, location) = DirectoryEntry::lookup(self, path)?;
        let location = location.ok_or(Error::IsRootDirectory)?;

        if let Some(accessed) = accessed {
            dirent.set_accessed(&accessed, self.variant);
        }
        if let Some(modified) = modified {
            dirent.set_modified(&modified, self.variant);
        }

        dirent.write_at(self, location)
    }

    /// Give a file a first cluster if it does not have one.
    ///
    /// Files this library creates always own a cluster, but an empty file
    /// written by another tool may have none, and a chain has to start
    /// somewhere before anything can be written to it.
    pub(crate) fn ensure_first_cluster(
        &mut self,
        dirent: &mut DirectoryEntry,
    ) -> Result<ClusterId, Error> {
        let cluster = dirent.first_cluster();
        if cluster != 0 {
            return Ok(cluster);
        }

        let cluster = self.alloc_cluster(true)?;
        dirent.set_first_cluster(cluster);
        Ok(cluster)
    }

    /// Walk a cluster chain to the cluster holding a byte offset.
    ///
    /// With `alloc` set, a chain that ends before the offset is extended to
    /// reach it, which is what writing past the end of a file needs.
    pub(crate) fn cluster_for_offset(
        &mut self,
        first_cluster: ClusterId,
        offset: u64,
        alloc: bool,
    ) -> Result<ClusterId, Error> {
        let mut cluster = first_cluster;

        for _ in 0..(offset / self.num_bytes_per_cluster) {
            cluster = match self.next_cluster(cluster)? {
                Some(next) => next,
                None if alloc => {
                    let next = self.alloc_cluster(true)?;
                    self.attach_cluster(cluster, next)?;
                    next
                }
                None => return Err(Error::InvalidClusterChain),
            };
        }

        Ok(cluster)
    }
}

pub struct FatxFsHandle {
    pub(crate) fs: Arc<Mutex<FatxFs>>, // FIXME: Make private
}

impl FatxFsHandle {
    pub(crate) fn with_lock<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut FatxFs) -> T,
    {
        let mut fs = self.fs.lock().unwrap();
        f(&mut fs)
    }

    pub fn open(&mut self, path: &str) -> Result<File, Error> {
        self.with_lock(|fs| fs.open(path))
    }

    pub fn stat(&mut self, path: &str) -> Result<DirectoryEntry, Error> {
        self.with_lock(|fs| fs.stat(path))
    }

    pub fn read_dir(&mut self, path: &str) -> Result<DirectoryEntryIntoIterator, Error> {
        self.with_lock(|fs| fs.read_dir(path))
    }

    /// Create an empty file and open it for writing.
    ///
    /// Fails if anything already exists at that path; use `open` for that.
    pub fn create(&mut self, path: &str) -> Result<File, Error> {
        self.with_lock(|fs| fs.create(path))
    }

    /// Create a directory.
    pub fn mkdir(&mut self, path: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.mkdir(path))
    }

    /// Remove a file.
    pub fn unlink(&mut self, path: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.unlink(path))
    }

    /// Remove an empty directory.
    pub fn rmdir(&mut self, path: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.rmdir(path))
    }

    /// Set the length of a file, padding with zeroes if it grows.
    pub fn truncate(&mut self, path: &str, size: u64) -> Result<(), Error> {
        self.with_lock(|fs| fs.truncate(path, size))
    }

    /// Move or rename a file or directory, replacing the destination if it
    /// already exists.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.rename(from, to))
    }

    /// Overwrite the access and modification timestamps of a node.
    pub fn set_times(
        &mut self,
        path: &str,
        accessed: Option<DateTime>,
        modified: Option<DateTime>,
    ) -> Result<(), Error> {
        self.with_lock(|fs| fs.set_times(path, accessed, modified))
    }

    /// Flush everything held in memory to the device.
    pub fn sync(&mut self) -> Result<(), Error> {
        self.with_lock(|fs| fs.sync())
    }

    /// Whether this filesystem was opened for writing.
    pub fn is_writable(&self) -> bool {
        self.with_lock(|fs| fs.writable)
    }

    /// The variant this filesystem was opened as, with Auto already resolved
    /// to whichever the signature turned out to be.
    pub fn variant(&self) -> Variant {
        self.with_lock(|fs| fs.variant)
    }
}
