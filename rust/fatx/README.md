fatx
====

Xbox and Xbox 360 FATX filesystem library.

Reading and writing are both supported: a filesystem opened with
`FatxFsConfig::writable(true)` can create, write, truncate, rename and remove
files and directories, on either flavour of the format.

```rust
use std::io::Write;
use fatx::{FatxFs, FatxFsConfig};

let config = FatxFsConfig::new("/dev/sdX".to_string())
    .drive_letter("e")
    .writable(true);
let mut fs = FatxFs::open_device(&config).unwrap();

fs.mkdir("/UDATA").unwrap();
let mut file = fs.create("/UDATA/SAVE.DAT").unwrap();
file.write_all(b"hello").unwrap();
file.flush().unwrap();
```

fatx-fuse
---------
If you want to mount the filesystem locally, check out fatx-fuse.

Windows
-------
Pass a raw device path — `\\.\PhysicalDrive1` for a whole disk, `\\.\D:` for a
volume — where a Unix build would take `/dev/sdX`. Everything else is the same;
the library aligns its own reads and writes to the device's sector size, which
Windows insists on and Unix does not care about.

Two things are worth knowing before the first call fails:

* **The process has to be elevated.** Windows refuses even read access to a raw
  device otherwise, and the error says so.
* **Windows must not have the disk mounted.** A FATX disk is `RAW` as far as
  Windows is concerned, so nothing claims it and writes go straight through; a
  disk carrying a volume Windows has mounted would refuse writes to that
  volume's sectors.

`cargo build` at the workspace root does not work on Windows, because
fatx-fuse needs libfuse. Build the library on its own:

```
cargo build -p fatx
```

macOS
-----
Pass `/dev/diskN` for the buffered node or `/dev/rdiskN` for the raw one;
`diskutil list` says which number the disk got. Both work, and both need
`sudo`. `rdiskN` bypasses the buffer cache and is about twice as quick at
copying a whole file in or out, so prefer it unless something else is looking
at the disk at the same time — the two nodes do not share a cache, and bytes
written through one are not guaranteed to be visible through the other until
the write reaches the media.

Neither node answers `stat` or `lseek(SEEK_END)` with anything but zero, so the
library asks the media for its size instead, and aligns its own access to the
block size on `rdiskN`, which rejects anything else. That is all handled for
you.

macOS also gates access to removable media behind a consent dialog, and a
process that has not been granted it **blocks** rather than being refused: a
read that never returns, with no error and nothing in the log, is this and not a
dead disk. Grant it under System Settings > Privacy & Security > Files and
Folders, for whichever program is doing the reading.

When the disk is plugged in, macOS offers *"The disk you attached was not
readable by this computer"*: choose **Ignore**. FATX is unknown to macOS, so
nothing mounts and writes go straight through. If some partition did mount,
unmount the whole disk first — `diskutil unmountDisk /dev/diskN` — or writes to
it are refused.

`cargo build` at the workspace root does not work on macOS either, because
fatx-fuse needs macFUSE. Build the library on its own:

```
cargo build -p fatx
```

fatxtool
--------
`examples/fatxtool.rs` is a small command line front end, which is the easiest
way to check a real disk where there is no FUSE mount to look at:

```
cargo run --example fatxtool -- \\.\PhysicalDrive1 --x360 data ls /
cargo run --example fatxtool -- \\.\PhysicalDrive1 --x360 data --rw put local.bin /remote.bin
cargo run --example fatxtool -- /dev/sdX --letter e df
cargo run --example fatxtool -- /dev/rdisk4 --x360 data df
```

Run it with no command for the list of options and commands.
