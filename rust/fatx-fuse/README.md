fatx-fuse
=========

[FUSE](https://en.wikipedia.org/wiki/Filesystem_in_Userspace) driver for the Original Xbox and Xbox 360 FATX filesystem.

Mounts read-only by default. Pass `--read-write` to create, write, rename and
remove files and directories:

```
fatx-fuse --read-write /dev/sdX /mnt/xbox
```
