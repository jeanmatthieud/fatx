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

fatxtool
--------
`examples/fatxtool.rs` is a small command line front end, which is the easiest
way to check a real disk where there is no FUSE mount to look at:

```
cargo run --example fatxtool -- \\.\PhysicalDrive1 --x360 data ls /
cargo run --example fatxtool -- \\.\PhysicalDrive1 --x360 data --rw put local.bin /remote.bin
cargo run --example fatxtool -- /dev/sdX --letter e df
```

Run it with no command for the list of options and commands.
