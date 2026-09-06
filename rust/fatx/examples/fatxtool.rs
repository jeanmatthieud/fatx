//! A small command line front end to the library, enough to look at a real
//! disk and to write to it.
//!
//! Kept as an example rather than a binary because the shipped tool is the
//! FUSE driver, which does not build on Windows; this is what is left when
//! there is no mount to work through.
//!
//! ```text
//! cargo run --example fatxtool -- <device> [options] <command> [args]
//!
//! options:
//!   --letter <x|y|z|c|e|f>   original Xbox partition (default: c)
//!   --x360 <name>            Xbox 360 partition: sysext, sysext2, compat, data
//!   --offset <bytes>         partition offset, overriding the layouts above
//!   --size <bytes>           partition size, overriding the layouts above
//!   --rw                     open the device for writing
//!
//! commands:
//!   df                       report free and used space
//!   ls <path>                list a directory
//!   stat <path>              show one entry
//!   cat <path> [dest]        copy a file out, to stdout or to dest
//!   put <source> <path>      copy a local file in
//!   mkdir <path>             create a directory
//!   rm <path>                delete a file
//!   rmdir <path>             delete an empty directory
//! ```

use std::io::{Read, Write};
use std::process::ExitCode;

use fatx::{FatxFs, FatxFsConfig};

fn main() -> ExitCode {
    env_logger::init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("fatxtool: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let device = args
        .next()
        .ok_or("usage: fatxtool <device> [options] <command> [args]")?;

    let mut config = FatxFsConfig::new(device).drive_letter("c");
    let mut command = Vec::new();

    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--letter" => config = config.drive_letter(&value()?),
            "--x360" => config = config.x360_partition(&value()?),
            "--offset" => config = config.partition_offset_bytes(parse_number(&value()?)?),
            "--size" => config = config.partition_size_bytes(parse_number(&value()?)?),
            "--rw" => config = config.writable(true),
            _ => {
                command.push(arg);
                command.extend(args);
                break;
            }
        }
    }

    let (command, operands) = command.split_first().ok_or("no command given")?;
    let mut fs = FatxFs::open_device(&config).map_err(|err| format!("cannot open: {err}"))?;

    let operand = |index: usize| -> Result<&str, String> {
        operands
            .get(index)
            .map(String::as_str)
            .ok_or_else(|| format!("{command} needs {} argument(s)", index + 1))
    };

    match command.as_str() {
        "df" => {
            let space = fs.space().map_err(stringify)?;
            println!("cluster size {} bytes", space.bytes_per_cluster);
            println!("total        {} bytes", space.total_bytes);
            println!("used         {} bytes", space.used_bytes());
            println!("free         {} bytes", space.free_bytes);
        }
        "ls" => {
            for entry in fs.read_dir(operand(0)?).map_err(stringify)? {
                let entry = entry.map_err(stringify)?;
                println!(
                    "{} {:>10} {}",
                    if entry.is_directory() { 'd' } else { '-' },
                    entry.file_size(),
                    entry.file_name()
                );
            }
        }
        "stat" => {
            let entry = fs.stat(operand(0)?).map_err(stringify)?;
            println!("name      {}", entry.file_name());
            println!(
                "kind      {}",
                if entry.is_directory() {
                    "directory"
                } else {
                    "file"
                }
            );
            println!("size      {}", entry.file_size());
        }
        "cat" => {
            let mut file = fs.open(operand(0)?).map_err(stringify)?;
            let mut data = Vec::new();
            file.read_to_end(&mut data).map_err(stringify)?;
            match operands.get(1) {
                Some(dest) => std::fs::write(dest, &data).map_err(stringify)?,
                None => std::io::stdout().write_all(&data).map_err(stringify)?,
            }
        }
        "put" => {
            let data = std::fs::read(operand(0)?).map_err(stringify)?;
            let path = operand(1)?;
            let mut file = fs.create(path).map_err(stringify)?;
            file.write_all(&data).map_err(stringify)?;
            file.flush().map_err(stringify)?;
            println!("wrote {} bytes to {path}", data.len());
        }
        "mkdir" => fs.mkdir(operand(0)?).map_err(stringify)?,
        "rm" => fs.unlink(operand(0)?).map_err(stringify)?,
        "rmdir" => fs.rmdir(operand(0)?).map_err(stringify)?,
        other => return Err(format!("unknown command {other}")),
    }

    fs.sync().map_err(stringify)?;
    Ok(())
}

fn stringify<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

/// Accept both `0x`-prefixed and plain decimal sizes, since the partition
/// layouts are always written in hex.
fn parse_number(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("{text} is not a number"))
}
