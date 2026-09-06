TODO: macOS
===========

The Rust library reads and writes FATX on Linux and on Windows. macOS has never
been run at all. This is what was done to get here, what is expected to work on
a Mac unchanged, what is expected to fail, and what to do about it — written for
whoever sits in front of a real Mac with a FATX disk attached.

Where things stand
------------------

| Platform | Path                  | State                                          |
|----------|-----------------------|------------------------------------------------|
| Linux    | `/dev/sdX`            | Works. The original target.                    |
| Windows  | `\\.\PhysicalDriveN`  | Works. Verified on an Xbox 360 250 GB disk.    |
| macOS    | `/dev/diskN`          | **Expected to work, unverified.**              |
| macOS    | `/dev/rdiskN`         | **Expected to fail.** Needs the module below.  |

The road so far
---------------

### What actually broke on Windows

FATX asks the device for whatever it happens to need: 4096 bytes of superblock,
a 64 byte directory entry, the single byte that marks an entry deleted. The
driver handed each of those straight to `std::fs::File`. Unix serves all of it
from the page cache, so nobody had ever noticed that the requests are neither
sector aligned nor whole sectors.

A Windows handle on a raw device rejects every one of them. Reading a directory
failed on the first entry. Separately, `seek(SeekFrom::End(0))` reports nothing
useful for a device handle, so a partition described as running to the end of
the disk could not be sized either.

### The fix

`src/device.rs` — a `Device` type that wraps `File` and implements `Read`,
`Write` and `Seek`, so the rest of the driver is unchanged and carries no
`#[cfg]` of its own. It keeps the caller's logical position, bounces unaligned
access through a sector buffer, read-modify-writes the partial head and tail
sectors of a write, and caches the last sector read — a directory is walked one
64 byte entry at a time and would otherwise read the same sector eight times.

When `sector_size` is 1 the whole bounce path is skipped and access goes
straight to the handle. That is the case for image files everywhere, and for
block devices outside Windows.

The platform-specific part is two small modules behind one interface:

```rust
pub(super) fn open(path: &Path, writable: bool) -> io::Result<File>;

/// The sector size and length of a raw device, or `None` for anything that
/// needs no alignment.
pub(super) fn raw_device_geometry(path: &Path, file: &File) -> io::Result<Option<(u64, u64)>>;
```

* `src/device/windows.rs` — shares the handle (`FILE_SHARE_READ|WRITE`, or the
  open is refused for a disk the system is holding), rewrites `PermissionDenied`
  into a message about elevation, and asks `IOCTL_DISK_GET_DRIVE_GEOMETRY` /
  `IOCTL_DISK_GET_LENGTH_INFO` for the sector size and the length. The geometry
  ioctl failing is how an image file is told apart from a device.
* `src/device/unix.rs` — opens the file and returns `None`. Nothing to do.

Commits: `8d72dfa` (the shim), `bee61cb` (see below).

### One regression, found and fixed while thinking about macOS

The first version of `Device::len()` used `metadata().len()`. That is right for
an image file — which is why the whole test suite stayed green — but **a block
device reports a size of zero from `stat`**. Both partitions described as
running to the end of the disk (`data` on the 360, `f` on the original Xbox)
would have failed to open with `InvalidPartitionSize`, *on Linux*, which is the
platform that worked before any of this.

`len()` now falls back to `seek(SeekFrom::End(0))`, exactly what the code did
before the shim existed. Windows never takes that branch: its length always
arrives from the ioctl as `Some`. This matters for macOS too — `/dev/diskN`
answers `lseek(SEEK_END)` and does not answer `stat`.

Why macOS is nearly there already
---------------------------------

macOS presents a disk twice:

* **`/dev/diskN`** is a buffered block device. Unaligned reads and writes are
  served from the buffer cache, and `lseek(SEEK_END)` gives the media size —
  the same contract as Linux. This should work today, through the existing
  `unix.rs` path, with no code change at all.
* **`/dev/rdiskN`** is the raw character device. It demands block-aligned,
  whole-block I/O: exactly the constraint that broke Windows. It currently takes
  the `unix.rs` path with alignment switched off, so it will fail — `EINVAL`,
  `Invalid argument`, on the very first directory entry.

`rdiskN` is worth having: it bypasses the buffer cache and is markedly faster
for the large sequential transfers a `put` or `cat` of a game does. But it is a
distinct piece of work, and writing ioctl code blind is guessing, so it was left
undone rather than shipped untested.

What to do on a real Mac
------------------------

### 0. Prerequisites

Rust from <https://rustup.rs>. Nothing else — the library has no C dependency.

Build the library alone, not the workspace: `fatx-fuse` needs macFUSE and will
otherwise stop the build before it starts.

```sh
cd rust
cargo build -p fatx --examples
```

Do not let the Mac try to make sense of the disk. It will offer *"The disk you
attached was not readable by this computer"* — choose **Ignore**. A FATX disk is
unknown to macOS, so nothing mounts on it and writes go straight through. If
some partition did get mounted, unmount the whole disk first, or writes to it
will be refused:

```sh
diskutil list                    # find the disk number
diskutil unmountDisk /dev/disk4  # only if something actually mounted
```

Everything below needs `sudo`.

### 1. The test suite should already pass

It runs entirely on image files, so it exercises the alignment logic without any
device at all. If this fails, the problem is not macOS-specific.

```sh
cargo test -p fatx
```

Expect 7 unit tests and 24 integration tests.

### 2. Prove that `/dev/diskN` takes unaligned I/O

Thirty seconds, and it decides everything that follows. `bs=1 skip=37` is an
unaligned single-byte read:

```sh
sudo dd if=/dev/disk4 bs=1 count=64 skip=37 2>/dev/null | xxd
sudo dd if=/dev/rdisk4 bs=1 count=64 skip=37 | xxd    # expected to fail
```

The first should print 64 bytes. The second is expected to report `Invalid
argument`; if it *succeeds*, macOS is more forgiving than assumed and step 4 may
not be needed at all — say so.

### 3. Read a real disk

Substitute the right disk number, and the right partition for the disk in hand
(`--x360 data` for a 360 disk, `--letter e` or `--letter f` for an original Xbox
disk; run `fatxtool` with no command for the full list).

```sh
sudo ../target/debug/examples/fatxtool /dev/disk4 --x360 data df
sudo ../target/debug/examples/fatxtool /dev/disk4 --x360 data ls /
```

`df` is the one that matters: `data` on a 360 is described as running to the end
of the disk, so a plausible total size proves `len()` got a real answer out of
`lseek(SEEK_END)` rather than the zero that `stat` would have given. A total of
0 bytes, or an `InvalidPartitionSize`, means macOS block devices do not answer
`SEEK_END` the way Linux does, and the length has to come from `DKIOCGETBLOCK*`
on `/dev/diskN` too.

### 4. Write, read back, clean up

Write only into a directory you created, and delete only what you wrote.

```sh
D=/dev/disk4
P="--x360 data"
T=../target/debug/examples/fatxtool

dd if=/dev/urandom of=/tmp/large.bin bs=1 count=102401   # not a whole number of sectors
echo -n "hello from a mac" > /tmp/small.txt

sudo $T $D $P --rw mkdir /mac-test
sudo $T $D $P --rw put /tmp/small.txt /mac-test/small.txt
sudo $T $D $P --rw put /tmp/large.bin /mac-test/large.bin
sudo $T $D $P ls /mac-test

# fresh process, so nothing cached in memory can flatter the result
sudo $T $D $P cat /mac-test/small.txt /tmp/small.back
sudo $T $D $P cat /mac-test/large.bin /tmp/large.back
shasum -a 256 /tmp/small.txt /tmp/small.back /tmp/large.bin /tmp/large.back

sudo $T $D $P --rw rm /mac-test/small.txt
sudo $T $D $P --rw rm /mac-test/large.bin
sudo $T $D $P --rw rmdir /mac-test
sudo $T $D $P df     # free space should return to its value from step 3
```

Both pairs of hashes must match. That is the same sequence that was run against
the Windows disk, where both matched and free space returned exactly.

If all of this passes, `/dev/diskN` is done: add macOS to the README next to the
Windows section and stop here unless `rdiskN` is wanted.

### 5. Only if `/dev/rdiskN` is wanted: `src/device/macos.rs`

The design already has the hole this drops into. In `src/device.rs`, split the
Unix arm:

```rust
#[cfg(target_os = "macos")]
#[path = "device/macos.rs"]
mod sys;

#[cfg(not(any(windows, target_os = "macos")))]
#[path = "device/unix.rs"]
mod sys;
```

`Cargo.toml`, so no other platform grows a dependency:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
libc = "0.2"
```

`src/device/macos.rs`: `open` is the same two lines as `unix.rs`.
`raw_device_geometry` returns `None` for everything except a **character**
device — `/dev/diskN` is a block device and must keep going down the unaligned
path, which is simpler and needs no ioctl. The distinction is in std, no libc
required:

```rust
use std::os::unix::fs::FileTypeExt;

if !file.metadata()?.file_type().is_char_device() {
    return Ok(None);
}
```

Then two ioctls, from `<sys/disk.h>`:

| Constant             | Value        | Yields              |
|----------------------|--------------|---------------------|
| `DKIOCGETBLOCKSIZE`  | `0x40046418` | `u32` block size    |
| `DKIOCGETBLOCKCOUNT` | `0x40086419` | `u64` block count   |

(`_IOR('d', 24, uint32_t)` and `_IOR('d', 25, uint64_t)`; recompute them from
the headers rather than trusting this table.) The length is the product of the
two. Reject a block size that is zero or not a power of two, as `windows.rs`
does. Note `libc::ioctl` on macOS takes `c_ulong` for the request, unlike Linux.

An ioctl failing here should be an error, not `Ok(None)`: unlike Windows, where
the failing geometry call is how an image file is recognised, this arm is only
reached for something already known to be a character device.

Nothing above `sys::` needs to change. The alignment, the bounce buffer and the
sector cache are already written and already tested.

Then re-run steps 1, 3 and 4 against `/dev/rdisk4`, and check the timing against
`/dev/disk4` — if `rdiskN` is not measurably faster on the large file, it is not
worth the extra module.

Open questions to answer on the machine
---------------------------------------

1. Does `lseek(SEEK_END)` on `/dev/diskN` return the media size on macOS? Step 3
   settles it. Everything else assumes yes.
2. Does a USB-SATA adapter present 512 or 4096 byte blocks? Only matters for
   step 5, and the ioctl reports it; but a 4096 byte block would also be the
   first real test of a sector size other than 512.
3. Is `/dev/rdiskN` actually faster here, enough to justify the module?

What to send back
-----------------

The output of steps 1 through 4, verbatim — particularly the `df` totals before
and after, and the four hashes. That is enough to close this out or to say
exactly what broke.
