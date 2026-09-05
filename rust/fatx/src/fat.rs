use std::io::{Seek, SeekFrom, Write};

use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::*;

use crate::error::Error;
use crate::variant::Variant;

#[derive(Debug)]
enum FatType {
    Type16,
    Type32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum FatEntry {
    Available,
    Reserved,
    Bad,
    Media,
    End,
    Data(u32),
    Invalid,
}

pub(crate) type FatEntryId = u32;
pub(crate) type ClusterId = u32;

impl From<u16> for FatEntry {
    fn from(value: u16) -> Self {
        match value {
            0x0000 => FatEntry::Available,
            0x0001..0xfff0 => FatEntry::Data(value as u32),
            0xfff0 => FatEntry::Reserved,
            0xfff7 => FatEntry::Bad,
            0xfff8 => FatEntry::Media,
            0xffff => FatEntry::End,
            _ => FatEntry::Invalid,
        }
    }
}

impl From<u32> for FatEntry {
    fn from(value: u32) -> Self {
        match value {
            0x00000000 => FatEntry::Available,
            0x00000001..0xfffffff0 => FatEntry::Data(value),
            0xfffffff0 => FatEntry::Reserved,
            0xfffffff7 => FatEntry::Bad,
            0xfffffff8 => FatEntry::Media,
            0xffffffff => FatEntry::End,
            _ => FatEntry::Invalid,
        }
    }
}

impl FatEntry {
    /// The 32-bit value this entry is written as.
    ///
    /// The markers are the same in both FAT widths once the narrow ones have
    /// been widened, so a FAT16 filesystem simply truncates the result.
    fn to_raw(self) -> Result<u32, Error> {
        Ok(match self {
            FatEntry::Available => 0x00000000,
            FatEntry::Reserved => 0xfffffff0,
            FatEntry::Bad => 0xfffffff7,
            FatEntry::Media => 0xfffffff8,
            FatEntry::End => 0xffffffff,
            FatEntry::Data(cluster) => {
                if cluster == 0 || cluster >= 0xfffffff0 {
                    return Err(Error::InvalidClusterNumber);
                }
                cluster
            }
            FatEntry::Invalid => return Err(Error::InvalidClusterChain),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Fat {
    fat_type: FatType,
    /// How many entries the FAT actually describes. The cache is rounded up to
    /// a whole number of blocks, so it is longer than that.
    num_entries: u32,
    pub(crate) fat_size_bytes: u64,
    pub(crate) fat_data: Vec<u8>, // FIXME: Smarter cache
    /// Half-open byte range of `fat_data` modified since the last flush.
    dirty: Option<(usize, usize)>,
}

impl Fat {
    pub(crate) fn new(num_fat_entries: u32) -> Self {
        // NOTE: this *MUST* be kept below the Cluster Reserved marker for FAT16
        let (fat_type, fat_size_bytes) = if num_fat_entries < 0xfff0 {
            (
                FatType::Type16,
                (num_fat_entries as u64 * 2).next_multiple_of(4096),
            )
        } else {
            (
                FatType::Type32,
                (num_fat_entries as u64 * 4).next_multiple_of(4096),
            )
        };

        let fat_data = vec![0u8; fat_size_bytes as usize];

        Self {
            fat_type,
            num_entries: num_fat_entries,
            fat_size_bytes,
            fat_data,
            dirty: None,
        }
    }

    /// Refuse a cluster number the FAT has no entry for.
    ///
    /// A cluster number read out of a directory entry is whatever the on-disk
    /// bytes say, so a corrupt or hand-made entry can point anywhere; without
    /// this the cache would simply be indexed out of bounds and the driver
    /// would panic.
    fn check_index(&self, index: FatEntryId) -> Result<(), Error> {
        if index >= self.num_entries {
            log::error!("Cluster {index} is outside the FAT");
            return Err(Error::InvalidClusterNumber);
        }
        Ok(())
    }

    /// The width, in bytes, of one entry of this FAT.
    fn entry_size(&self) -> usize {
        match self.fat_type {
            FatType::Type16 => 2,
            FatType::Type32 => 4,
        }
    }

    /// Read a FAT entry.
    ///
    /// The cached FAT is held in on-disk byte order, so entries are swapped
    /// here as they are read out rather than when the cache is filled.
    pub(crate) fn entry(&mut self, index: FatEntryId, variant: Variant) -> Result<FatEntry, Error> {
        self.check_index(index)?;
        let swap = variant.needs_swap();
        match self.fat_type {
            FatType::Type16 => {
                let fat = <[U16]>::ref_from_bytes_with_elems(
                    &self.fat_data[..],
                    (self.fat_size_bytes / 2) as usize,
                )
                .unwrap();
                let value: u16 = fat[index as usize].into();
                let value = if swap { value.swap_bytes() } else { value };
                Ok(value.into())
            }
            FatType::Type32 => {
                let fat = <[U32]>::ref_from_bytes_with_elems(
                    &self.fat_data[..],
                    (self.fat_size_bytes / 4) as usize,
                )
                .unwrap();
                let value: u32 = fat[index as usize].into();
                let value = if swap { value.swap_bytes() } else { value };
                Ok(value.into())
            }
        }
    }

    /// Write a FAT entry into the cache, to be flushed later.
    ///
    /// As with reading, the cache is kept in on-disk byte order, so the value
    /// is swapped on its way in.
    pub(crate) fn set_entry(
        &mut self,
        index: FatEntryId,
        entry: FatEntry,
        variant: Variant,
    ) -> Result<(), Error> {
        self.check_index(index)?;
        let swap = variant.needs_swap();
        let raw = entry.to_raw()?;
        let offset = index as usize * self.entry_size();

        match self.fat_type {
            FatType::Type16 => {
                let value = raw as u16;
                let value = if swap { value.swap_bytes() } else { value };
                let fat = <[U16]>::mut_from_bytes_with_elems(
                    &mut self.fat_data[..],
                    (self.fat_size_bytes / 2) as usize,
                )
                .unwrap();
                fat[index as usize] = value.into();
            }
            FatType::Type32 => {
                let value = if swap { raw.swap_bytes() } else { raw };
                let fat = <[U32]>::mut_from_bytes_with_elems(
                    &mut self.fat_data[..],
                    (self.fat_size_bytes / 4) as usize,
                )
                .unwrap();
                fat[index as usize] = value.into();
            }
        }

        let end = offset + self.entry_size();
        self.dirty = Some(match self.dirty {
            Some((start, prev_end)) => (start.min(offset), prev_end.max(end)),
            None => (offset, end),
        });

        Ok(())
    }

    /// Write the modified part of the cached FAT back to the device.
    ///
    /// Only the range touched since the last flush is written, so a single
    /// entry change does not rewrite a FAT that may be megabytes long.
    pub(crate) fn flush<D: Write + Seek>(
        &mut self,
        device: &mut D,
        fat_offset_bytes: u64,
    ) -> Result<(), Error> {
        let Some((start, end)) = self.dirty else {
            return Ok(());
        };

        device.seek(SeekFrom::Start(fat_offset_bytes + start as u64))?;
        device.write_all(&self.fat_data[start..end])?;
        self.dirty = None;

        Ok(())
    }
}
