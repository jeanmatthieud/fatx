macOS
=====

The Rust library reads and writes FATX on Linux, on Windows and on macOS. This
is what the macOS work found, what was verified and how, and the one question
left open.

Where things stand
------------------

| Platform | Path                  | State                                          |
|----------|-----------------------|------------------------------------------------|
| Linux    | `/dev/sdX`            | Works. The original target.                    |
| Windows  | `\\.\PhysicalDriveN`  | Works. Verified on an Xbox 360 250 GB disk.    |
| macOS    | `/dev/diskN`          | Works. Verified on the same 250 GB disk.       |
| macOS    | `/dev/rdiskN`         | Works. Verified on the same 250 GB disk.       |

One thing to know before the first read hangs rather than fails: macOS gates
access to removable media behind a consent dialog, and a process that has not
been granted it **blocks** rather than being refused. A `dd` of a single sector
sat there for ten minutes with no kernel log entry and no error, which looks
exactly like dead hardware and is not. If a read never returns, look for the
permission prompt — System Settings › Privacy & Security › Files and Folders,
for whichever program is doing the reading.

What the previous round assumed, and what was actually true
-----------------------------------------------------------

The plan written before anyone had run this on a Mac had `/dev/diskN` working
unchanged and only `/dev/rdiskN` needing a new module. Half of that was right.

**Wrong: `lseek(SEEK_END)` does not report the media size.** It returns zero, on
the buffered node *and* the raw one, exactly as `stat` does:

```
/dev/disk13:  st_size=0  lseek(SEEK_END)=0
/dev/rdisk13: st_size=0  lseek(SEEK_END)=0
```

So `/dev/diskN` did not work unchanged at all. Every partition described as
running to the end of the disk — `data` on a 360, `f` on an original Xbox —
failed at open with `the partition size is invalid`, because `Device::len()`
fell through to the seek and got a zero. macOS needs the ioctl for the length
whichever node it is handed, which is the same hole Windows leaves.

**Right: `/dev/rdiskN` demands aligned whole-block access.** Confirmed on the
disk before touching any code:

```
$ dd if=/dev/disk13  bs=1 count=64 skip=37   # 64 bytes, fine
$ dd if=/dev/rdisk13 bs=1 count=64 skip=37
dd: /dev/rdisk13: Invalid argument
```

**Right: the ioctl numbers.** `DKIOCGETBLOCKSIZE` is `0x40046418` and
`DKIOCGETBLOCKCOUNT` is `0x40086419`, recomputed from `_IOR('d', 24, uint32_t)`
and `_IOR('d', 25, uint64_t)` and then checked against the running kernel. Both
answer on both nodes, and their product is the media size to the byte:
`512 x 488397168 = 250059350016`, which is what `diskutil info` reports for the
disk. They are served from the IOKit registry and never touch the media.

**Not foreseen: `sync_all` fails on every device node.** `File::sync_all` issues
`fcntl(F_FULLFSYNC)` on macOS, which no device node implements; it fails with
`ENOTTY`, and the fallback in std only covers `ENOTSUP`. Every write ended in
`fatxtool: io error` even though the write itself had landed — the directory was
there afterwards. Plain `fsync` works on both nodes and is what there is to ask
for: it flushes the buffer cache, which is all that stands between the caller
and the media on `diskN` and is not in the path at all on `rdiskN`.

What changed
------------

`src/device/macos.rs`, a third arm of the platform layer that was already there
for Windows, plus a `sync` hook added to that interface so the `F_FULLFSYNC`
problem is handled where the rest of the platform differences are. Nothing above
`sys::` changed; the alignment, the bounce buffer and the sector cache were
already written and already tested.

* `raw_device_geometry` decides on the file type rather than on a failing ioctl:
  a regular file is an image and returns `None`, a character or block device is
  asked for its geometry, and an ioctl failing there is an error rather than a
  silent fall-through.
* A **character** device (`rdiskN`) returns its real block size, so the
  alignment machinery engages. A **block** device (`diskN`) returns an alignment
  of one — the buffer cache serves any offset and any length, so the bounce
  buffer would only cost — but still returns the length. That combination,
  no alignment yet a known length, is new; it is what `Device` needs and it
  already handled it.
* `sync` retries as `fsync` when `F_FULLFSYNC` comes back `ENOTTY`. An image
  file still gets the full barrier, which is the stronger call and the right one
  there.

No new dependency: `ioctl` and `fsync` are declared in the module the same way
`windows.rs` declares `DeviceIoControl`.

How it was verified
-------------------

On the Xbox 360 250 GB disk, on a JMicron USB-SATA bridge, on macOS 15.7.3
(Apple silicon). Every run below was done on `/dev/disk13` and on `/dev/rdisk13`
in turn, and the two agreed on everything.

* `cargo test -p fatx` — 8 unit tests and 24 integration tests, on image files,
  which is where the alignment logic is actually pinned down.
* `df` and `ls /`: 244,883,849,216 bytes total, 116,306,608,128 used, and the
  same listing of a real game library from both nodes. `data` is the partition
  described as running to the end of the disk, so this is the length path that
  was broken; before the fix it was `the partition size is invalid`.
* The full write sequence — `mkdir /mac-test`, `put` of a 16 byte file and of a
  102,401 byte one (deliberately not a whole number of sectors), `ls`, `cat`
  back out in a fresh process, `rm`, `rm`, `rmdir`. Both SHA-256 pairs matched
  on both nodes, free space came back to 128,577,241,088 bytes exactly, and
  nothing was left behind.
* A 100 MiB file put and read back, hashes matching, on both nodes.

Also verified against a file-backed pair of nodes — a sparse container with the
image from `tests/make_x360_image.py` at the 360 `data` offset, attached with
`hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount` — which is a
cheaper way to exercise the same code with no disk to hand, and which agreed
with the hardware throughout.

Is `rdiskN` worth the module?
-----------------------------

Yes, comfortably. On the disk above, 100 MiB each way:

|                        | `/dev/disk13` | `/dev/rdisk13` |
|------------------------|---------------|----------------|
| `dd`, kernel only      | 20.8 MB/s     | 80.2 MB/s      |
| library, `cat` out     | 7.95 s        | 3.83 s         |
| library, `put` in      | 12.12 s       | 9.41 s         |

The kernel path alone is nearly four times quicker, and reading a file through
the library is twice as quick even with the bounce buffer and the sector cache
in the way. Writing gains least, which is what you would expect: the buffered
node gets to return before the media has it.

What is left
------------

**A block size other than 512.** Every node seen so far reports 512 — the
JMicron bridge included — so the alignment path has still only ever run at that
size. A 4096 byte device would be the first real test of another. The ioctl
already reports it and the arithmetic is in terms of it, so this is a matter of
finding such a disk rather than of writing anything.
