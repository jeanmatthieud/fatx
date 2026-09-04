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
