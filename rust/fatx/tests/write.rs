//! Write-path tests against synthetic images of both flavours.
//!
//! The images are assembled here byte by byte from the format spec rather than
//! by the library's own writer, and the results of the 360 writes are decoded
//! again by hand at the end. Round-tripping through the library alone would not
//! prove much: a writer that byte-swaps consistently but wrongly reads back
//! everything it wrote and looks perfectly healthy.

use std::io::{Read, Seek, SeekFrom, Write};

use fatx::{FatxFs, FatxFsConfig, FatxFsHandle, Variant};

const SECTOR_SIZE: u64 = 512;
const SECTORS_PER_CLUSTER: u32 = 32;
const BYTES_PER_CLUSTER: u64 = SECTOR_SIZE * SECTORS_PER_CLUSTER as u64; // 16 KiB
const PARTITION_SIZE: u64 = 16 * 1024 * 1024;
const SUPERBLOCK_SIZE: usize = 4096;
const FAT_OFFSET: usize = 4096;
const FAT_SIZE: usize = 4096;
const CLUSTER_OFFSET: u64 = FAT_OFFSET as u64 + FAT_SIZE as u64;
const DIRENT_SIZE: usize = 64;
const ENTRIES_PER_CLUSTER: usize = (BYTES_PER_CLUSTER / DIRENT_SIZE as u64) as usize;

const SIGNATURE: u32 = 0x5854_4146; // 'FATX' little-endian, 'XTAF' big-endian
const ROOT_CLUSTER: u32 = 1;
const FAT16_MEDIA: u16 = 0xfff8;
const FAT16_END: u16 = 0xffff;
const END_OF_DIR_MARKER: u8 = 0xff;

/// A temporary image file, removed when the test drops it.
struct Image {
    path: std::path::PathBuf,
}

impl Image {
    /// Build a blank filesystem of the given flavour: a superblock, a FAT with
    /// nothing but the media descriptor and the root directory in it, and a
    /// root cluster holding a single end-of-directory marker.
    fn new(name: &str, variant: Variant) -> Self {
        let mut image = vec![0u8; PARTITION_SIZE as usize];

        let mut superblock = Vec::new();
        // The signature is written in the disk's own byte order: the bytes
        // spell FATX little-endian and XTAF big-endian, and both read back as
        // the same word, which is how the flavour is told apart.
        superblock.extend_from_slice(&to_disk_u32(SIGNATURE, variant));
        for value in [0xcafe_babeu32, SECTORS_PER_CLUSTER, ROOT_CLUSTER] {
            superblock.extend_from_slice(&to_disk_u32(value, variant));
        }
        superblock.extend_from_slice(&to_disk_u16(0, variant));
        superblock.resize(SUPERBLOCK_SIZE, 0xff);
        image[..SUPERBLOCK_SIZE].copy_from_slice(&superblock);

        let mut fat = vec![0u8; FAT_SIZE];
        fat[0..2].copy_from_slice(&to_disk_u16(FAT16_MEDIA, variant));
        fat[2..4].copy_from_slice(&to_disk_u16(FAT16_END, variant));
        image[FAT_OFFSET..FAT_OFFSET + FAT_SIZE].copy_from_slice(&fat);

        image[cluster_at(ROOT_CLUSTER) as usize] = END_OF_DIR_MARKER;

        let path = std::env::temp_dir().join(format!(
            "fatx-write-{name}-{:?}-{}.img",
            variant,
            std::process::id()
        ));
        std::fs::write(&path, &image).unwrap();
        Self { path }
    }

    fn path(&self) -> String {
        self.path.to_str().unwrap().to_string()
    }

    fn open(&self, variant: Variant, writable: bool) -> FatxFsHandle {
        let config = FatxFsConfig::new(self.path())
            .variant(variant)
            .writable(writable)
            .partition_offset_bytes(0)
            .partition_size_bytes(PARTITION_SIZE);
        FatxFs::open_device(&config).unwrap()
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap()
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn to_disk_u16(value: u16, variant: Variant) -> [u8; 2] {
    match variant {
        Variant::X360 => value.to_be_bytes(),
        _ => value.to_le_bytes(),
    }
}

fn to_disk_u32(value: u32, variant: Variant) -> [u8; 4] {
    match variant {
        Variant::X360 => value.to_be_bytes(),
        _ => value.to_le_bytes(),
    }
}

fn cluster_at(cluster: u32) -> u64 {
    CLUSTER_OFFSET + (cluster as u64 - 1) * BYTES_PER_CLUSTER
}

fn write_file(fs: &mut FatxFsHandle, path: &str, data: &[u8]) {
    let mut file = fs.create(path).unwrap();
    file.write_all(data).unwrap();
    file.flush().unwrap();
}

fn read_file(fs: &mut FatxFsHandle, path: &str) -> Vec<u8> {
    let mut file = fs.open(path).unwrap();
    let mut out = Vec::new();
    file.read_to_end(&mut out).unwrap();
    out
}

fn listing(fs: &mut FatxFsHandle, path: &str) -> Vec<String> {
    let mut names: Vec<String> = fs
        .read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

/// Every test runs against both flavours; only the byte order on disk differs.
fn both_variants() -> [Variant; 2] {
    [Variant::Xbox, Variant::X360]
}

#[test]
fn a_written_file_reads_back_and_survives_a_remount() {
    for variant in both_variants() {
        let image = Image::new("roundtrip", variant);
        let payload = b"Hello from the Rust write path.\n";

        let mut fs = image.open(variant, true);
        write_file(&mut fs, "/HELLO.TXT", payload);
        assert_eq!(read_file(&mut fs, "/HELLO.TXT"), payload);
        drop(fs);

        // Reopen from scratch, so nothing is being served out of memory.
        let mut fs = image.open(variant, false);
        assert_eq!(
            fs.stat("/HELLO.TXT").unwrap().file_size(),
            payload.len() as u32
        );
        assert_eq!(read_file(&mut fs, "/HELLO.TXT"), payload);
        assert_eq!(listing(&mut fs, "/"), vec!["HELLO.TXT"]);
    }
}

#[test]
fn a_file_larger_than_a_cluster_spans_the_chain() {
    for variant in both_variants() {
        let image = Image::new("chain", variant);
        // Deliberately not a whole number of clusters, so the last one is part
        // used and the file size has to be respected on the way back.
        let payload: Vec<u8> = (0..(BYTES_PER_CLUSTER as usize * 3 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();

        let mut fs = image.open(variant, true);
        write_file(&mut fs, "/BIG.BIN", &payload);
        drop(fs);

        let mut fs = image.open(variant, false);
        assert_eq!(read_file(&mut fs, "/BIG.BIN"), payload);
    }
}

#[test]
fn writing_in_pieces_and_seeking_back_lands_in_the_right_place() {
    for variant in both_variants() {
        let image = Image::new("pieces", variant);
        let mut fs = image.open(variant, true);

        let mut file = fs.create("/PIECES.BIN").unwrap();
        file.write_all(&[0xaa; 100]).unwrap();
        file.write_all(&[0xbb; 100]).unwrap();
        file.seek(SeekFrom::Start(50)).unwrap();
        file.write_all(&[0xcc; 10]).unwrap();
        file.flush().unwrap();
        drop(file);

        let contents = read_file(&mut fs, "/PIECES.BIN");
        assert_eq!(contents.len(), 200);
        assert!(contents[..50].iter().all(|&b| b == 0xaa));
        assert!(contents[50..60].iter().all(|&b| b == 0xcc));
        assert!(contents[60..100].iter().all(|&b| b == 0xaa));
        assert!(contents[100..].iter().all(|&b| b == 0xbb));
    }
}

#[test]
fn writing_past_the_end_of_a_file_fills_the_gap_with_zeroes() {
    for variant in both_variants() {
        let image = Image::new("gap", variant);
        let mut fs = image.open(variant, true);

        let mut file = fs.create("/SPARSE.BIN").unwrap();
        file.write_all(b"start").unwrap();
        // Past the end of the first cluster, so the gap spans an allocation.
        let gap_end = BYTES_PER_CLUSTER + 42;
        file.seek(SeekFrom::Start(gap_end)).unwrap();
        file.write_all(b"end").unwrap();
        file.flush().unwrap();
        drop(file);

        let contents = read_file(&mut fs, "/SPARSE.BIN");
        assert_eq!(contents.len() as u64, gap_end + 3);
        assert_eq!(&contents[..5], b"start");
        assert!(contents[5..gap_end as usize].iter().all(|&b| b == 0));
        assert_eq!(&contents[gap_end as usize..], b"end");
    }
}

#[test]
fn directories_hold_files_and_nest() {
    for variant in both_variants() {
        let image = Image::new("dirs", variant);
        let mut fs = image.open(variant, true);

        fs.mkdir("/GAMES").unwrap();
        fs.mkdir("/GAMES/HALO").unwrap();
        write_file(&mut fs, "/GAMES/HALO/SAVE.DAT", b"save data");
        write_file(&mut fs, "/GAMES/COVER.JPG", b"cover");
        drop(fs);

        let mut fs = image.open(variant, false);
        assert_eq!(listing(&mut fs, "/"), vec!["GAMES"]);
        assert_eq!(listing(&mut fs, "/GAMES"), vec!["COVER.JPG", "HALO"]);
        assert_eq!(listing(&mut fs, "/GAMES/HALO"), vec!["SAVE.DAT"]);
        assert_eq!(read_file(&mut fs, "/GAMES/HALO/SAVE.DAT"), b"save data");
        assert!(fs.stat("/GAMES").unwrap().is_directory());
    }
}

#[test]
fn a_directory_grows_beyond_one_cluster() {
    for variant in both_variants() {
        let image = Image::new("bigdir", variant);
        let mut fs = image.open(variant, true);

        // One more than fits in a cluster, so the directory has to be extended.
        let count = ENTRIES_PER_CLUSTER + 5;
        for i in 0..count {
            write_file(
                &mut fs,
                &format!("/FILE{i:04}.TXT"),
                format!("{i}").as_bytes(),
            );
        }
        drop(fs);

        let mut fs = image.open(variant, false);
        assert_eq!(listing(&mut fs, "/").len(), count);
        assert_eq!(read_file(&mut fs, "/FILE0260.TXT"), b"260");
    }
}

#[test]
fn removing_a_file_frees_its_space_for_the_next_one() {
    for variant in both_variants() {
        let image = Image::new("unlink", variant);
        let mut fs = image.open(variant, true);

        let payload = vec![0x5au8; BYTES_PER_CLUSTER as usize * 2];
        write_file(&mut fs, "/GONE.BIN", &payload);
        let first_cluster_offset = cluster_at(2);

        fs.unlink("/GONE.BIN").unwrap();
        assert!(fs.stat("/GONE.BIN").is_err());
        assert_eq!(listing(&mut fs, "/"), Vec::<String>::new());

        // The freed clusters come back, and the deleted entry's slot is reused.
        write_file(&mut fs, "/NEW.BIN", b"new");
        drop(fs);

        let mut fs = image.open(variant, false);
        assert_eq!(read_file(&mut fs, "/NEW.BIN"), b"new");
        assert_eq!(listing(&mut fs, "/"), vec!["NEW.BIN"]);
        assert_eq!(fs.stat("/NEW.BIN").unwrap().file_size(), 3);
        // The new file landed in the cluster the old one had held.
        assert_eq!(&image.bytes()[first_cluster_offset as usize..][..3], b"new");
    }
}

#[test]
fn a_directory_can_only_be_removed_once_it_is_empty() {
    for variant in both_variants() {
        let image = Image::new("rmdir", variant);
        let mut fs = image.open(variant, true);

        fs.mkdir("/DIR").unwrap();
        write_file(&mut fs, "/DIR/FILE.TXT", b"x");
        assert!(fs.rmdir("/DIR").is_err());

        fs.unlink("/DIR/FILE.TXT").unwrap();
        fs.rmdir("/DIR").unwrap();
        assert!(fs.stat("/DIR").is_err());
    }
}

#[test]
fn truncation_grows_with_zeroes_and_shrinks_without_disturbing_the_head() {
    for variant in both_variants() {
        let image = Image::new("truncate", variant);
        let mut fs = image.open(variant, true);

        let payload: Vec<u8> = (0..(BYTES_PER_CLUSTER as usize * 2))
            .map(|i| (i % 251) as u8)
            .collect();
        write_file(&mut fs, "/T.BIN", &payload);

        fs.truncate("/T.BIN", 100).unwrap();
        assert_eq!(read_file(&mut fs, "/T.BIN"), payload[..100]);

        let grown = BYTES_PER_CLUSTER + 7;
        fs.truncate("/T.BIN", grown).unwrap();
        let contents = read_file(&mut fs, "/T.BIN");
        assert_eq!(contents.len() as u64, grown);
        assert_eq!(&contents[..100], &payload[..100]);
        // Everything past the old end must read as zero, including the tail of
        // the cluster that still holds the data written before the truncation.
        assert!(contents[100..].iter().all(|&b| b == 0));
    }
}

#[test]
fn renaming_moves_an_entry_without_moving_its_contents() {
    for variant in both_variants() {
        let image = Image::new("rename", variant);
        let mut fs = image.open(variant, true);

        write_file(&mut fs, "/A.TXT", b"contents of a");
        fs.mkdir("/SUB").unwrap();
        let size = fs.stat("/A.TXT").unwrap().file_size();

        fs.rename("/A.TXT", "/SUB/B.TXT").unwrap();
        assert!(fs.stat("/A.TXT").is_err());
        assert_eq!(read_file(&mut fs, "/SUB/B.TXT"), b"contents of a");
        assert_eq!(fs.stat("/SUB/B.TXT").unwrap().file_size(), size);

        // Renaming over an existing file replaces it.
        write_file(&mut fs, "/C.TXT", b"contents of c");
        fs.rename("/C.TXT", "/SUB/B.TXT").unwrap();
        assert_eq!(read_file(&mut fs, "/SUB/B.TXT"), b"contents of c");
        assert_eq!(listing(&mut fs, "/SUB"), vec!["B.TXT"]);
    }
}

#[test]
fn a_read_only_filesystem_refuses_every_change() {
    for variant in both_variants() {
        let image = Image::new("readonly", variant);
        let mut fs = image.open(variant, true);
        write_file(&mut fs, "/A.TXT", b"a");
        drop(fs);

        let mut fs = image.open(variant, false);
        assert!(!fs.is_writable());
        assert!(fs.create("/B.TXT").is_err());
        assert!(fs.mkdir("/D").is_err());
        assert!(fs.unlink("/A.TXT").is_err());
        assert!(fs.truncate("/A.TXT", 0).is_err());

        let mut file = fs.open("/A.TXT").unwrap();
        assert!(file.write_all(b"nope").is_err());

        // Nothing of the above reached the disk.
        drop(fs);
        let mut fs = image.open(variant, false);
        assert_eq!(read_file(&mut fs, "/A.TXT"), b"a");
        assert_eq!(listing(&mut fs, "/"), vec!["A.TXT"]);
    }
}

#[test]
fn creating_something_that_already_exists_is_refused() {
    for variant in both_variants() {
        let image = Image::new("exists", variant);
        let mut fs = image.open(variant, true);

        write_file(&mut fs, "/A.TXT", b"a");
        assert!(fs.create("/A.TXT").is_err());
        assert!(fs.mkdir("/A.TXT").is_err());
        fs.mkdir("/D").unwrap();
        assert!(fs.mkdir("/D").is_err());
        // The refusals left the original alone.
        assert_eq!(read_file(&mut fs, "/A.TXT"), b"a");
    }
}

#[test]
fn a_name_too_long_for_the_field_is_refused() {
    let image = Image::new("longname", Variant::Xbox);
    let mut fs = image.open(Variant::Xbox, true);

    let longest = "A".repeat(42);
    fs.create(&format!("/{longest}")).unwrap();
    assert_eq!(listing(&mut fs, "/"), vec![longest]);

    assert!(fs.create(&format!("/{}", "A".repeat(43))).is_err());
}

/// Decode what the library wrote to a 360 image without going through the
/// library, so that a consistently wrong byte order cannot pass unnoticed.
#[test]
fn what_the_writer_puts_on_a_360_disk_decodes_big_endian() {
    let image = Image::new("bigendian", Variant::X360);
    let payload = b"big endian payload";

    let mut fs = image.open(Variant::X360, true);
    write_file(&mut fs, "/HELLO.TXT", payload);
    fs.mkdir("/GAMES").unwrap();
    drop(fs);

    let bytes = image.bytes();

    // The FAT: entry 2 belongs to the file and must end its chain.
    let fat_entry = |index: usize| {
        u16::from_be_bytes(
            bytes[FAT_OFFSET + index * 2..FAT_OFFSET + index * 2 + 2]
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(fat_entry(0), FAT16_MEDIA);
    assert_eq!(fat_entry(1), FAT16_END, "root directory");
    assert_eq!(fat_entry(2), FAT16_END, "the file, one cluster long");
    assert_eq!(fat_entry(3), FAT16_END, "the directory, one cluster long");

    // The root directory's first entry, decoded by hand.
    let dirent = &bytes[cluster_at(ROOT_CLUSTER) as usize..][..DIRENT_SIZE];
    assert_eq!(dirent[0] as usize, "HELLO.TXT".len());
    assert_eq!(dirent[1], 0, "a plain file has no attributes set");
    assert_eq!(&dirent[2..11], b"HELLO.TXT");
    assert!(
        dirent[11..44].iter().all(|&b| b == 0xff),
        "the unused tail of the name should be padded"
    );
    assert_eq!(u32::from_be_bytes(dirent[44..48].try_into().unwrap()), 2);
    assert_eq!(
        u32::from_be_bytes(dirent[48..52].try_into().unwrap()),
        payload.len() as u32
    );

    // The date comes before the time on this console, and both must be
    // non-zero: a stamp of zero would decode as a valid date either way round.
    let date = u16::from_be_bytes(dirent[52..54].try_into().unwrap());
    let time = u16::from_be_bytes(dirent[54..56].try_into().unwrap());
    let year = ((date >> 9) & 0x7f) + 1980;
    let month = (date >> 5) & 0xf;
    let day = date & 0x1f;
    assert!((2020..2100).contains(&year), "year {year} out of range");
    assert!((1..=12).contains(&month), "month {month} out of range");
    assert!((1..=31).contains(&day), "day {day} out of range");
    assert!((time >> 11) < 24, "hour out of range");
    assert!(((time >> 5) & 0x3f) < 60, "minute out of range");

    // The directory entry that follows it, and the end marker after that.
    let dirent = &bytes[cluster_at(ROOT_CLUSTER) as usize + DIRENT_SIZE..][..DIRENT_SIZE];
    assert_eq!(&dirent[2..7], b"GAMES");
    assert_eq!(dirent[1], 0x10, "directory attribute");
    assert_eq!(u32::from_be_bytes(dirent[44..48].try_into().unwrap()), 3);
    assert_eq!(
        bytes[cluster_at(ROOT_CLUSTER) as usize + 2 * DIRENT_SIZE],
        END_OF_DIR_MARKER
    );

    // The file's data goes down as a plain byte stream, with no swapping.
    assert_eq!(&bytes[cluster_at(2) as usize..][..payload.len()], payload);
    // And the new directory is empty.
    assert_eq!(bytes[cluster_at(3) as usize], END_OF_DIR_MARKER);
}

#[test]
fn a_position_past_the_largest_possible_file_is_refused() {
    let image = Image::new("hugeseek", Variant::Xbox);
    let mut fs = image.open(Variant::Xbox, true);

    let mut file = fs.create("/A.BIN").unwrap();
    file.write_all(b"head").unwrap();

    // A file's size is a 32 bit field. A position past it used to be truncated
    // into a valid one, which wrote over the start of the file and reported
    // success.
    assert!(file.seek(SeekFrom::Start(u32::MAX as u64 + 10)).is_err());
    file.flush().unwrap();
    drop(file);

    assert_eq!(read_file(&mut fs, "/A.BIN"), b"head");
}

#[test]
fn a_file_that_owns_no_cluster_can_still_be_removed() {
    for variant in both_variants() {
        let image = Image::new("nocluster", variant);
        let mut fs = image.open(variant, true);
        write_file(&mut fs, "/EMPTY.DAT", b"");
        drop(fs);

        // An empty file written by another tool need not point at a cluster at
        // all, which this library's own writer never produces.
        let first_cluster_field = cluster_at(ROOT_CLUSTER) as usize + 44;
        let mut bytes = image.bytes();
        bytes[first_cluster_field..first_cluster_field + 4].fill(0);
        std::fs::write(&image.path, &bytes).unwrap();

        let mut fs = image.open(variant, true);
        assert_eq!(fs.stat("/EMPTY.DAT").unwrap().file_size(), 0);
        fs.unlink("/EMPTY.DAT").unwrap();
        assert_eq!(listing(&mut fs, "/"), Vec::<String>::new());
    }
}

#[test]
fn reading_a_file_the_image_is_too_short_to_hold_does_not_hang() {
    let image = Image::new("shortimage", Variant::Xbox);
    let payload = vec![0x42u8; BYTES_PER_CLUSTER as usize];

    let mut fs = image.open(Variant::Xbox, true);
    write_file(&mut fs, "/BIG.BIN", &payload);
    drop(fs);

    // Cut the image off part way through the file's only cluster, as a
    // truncated dump would be. The entry still claims the full size.
    let cut = cluster_at(2) + 512;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&image.path)
        .unwrap()
        .set_len(cut)
        .unwrap();

    let mut fs = image.open(Variant::Xbox, false);
    let contents = read_file(&mut fs, "/BIG.BIN");
    assert_eq!(contents.len(), 512, "only what the image actually holds");
    assert!(contents.iter().all(|&b| b == 0x42));
}
