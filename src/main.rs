//! image-worker — single core, two subprograms:
//! `read` (image-inspect compatible) and `write` (image-repack compatible).
//!
//! Usage:
//!   image-worker read [OPTIONS] <image|-> [path]
//!   image-worker write <input> <output> ACTION... [OPTIONS]
//!
//! For drop-in compatibility the subcommand may be omitted: arguments
//! containing repack actions are treated as `write`, everything else as `read`.

mod core;
mod fs;
mod read;
mod write;

fn print_usage() {
    eprintln!("image-worker - inspect (read) and repack (write) Android filesystem images");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  image-worker read [OPTIONS] <image|-> [path]");
    eprintln!("  image-worker write <input> <output> ACTION... [OPTIONS]");
    eprintln!();
    eprintln!("  read   inspect images without mounting (image-inspect compatible):");
    eprintln!("         -i/--info, -l/--ls, -f/--find [pattern], -c/--cat,");
    eprintln!("         -t/-F/-o/-p/-n/-s/-H/-Z metadata, --stdin, -h/--help");
    eprintln!("  write  queue and apply image actions (image-repack compatible):");
    eprintln!("         --add/--cp/--mv/--rm/-R/--set-mode/--set-owner/--set-context,");
    eprintln!("         --mode/--uid/--gid/--context, --sparse-output,");
    eprintln!("         --shared-blocks y|n, --reserve-mb N, --compact,");
    eprintln!("         --compress lz4|deflate|none, --convert-to erofs|ext4,");
    eprintln!("         --input/--output, - as stdin/stdout");
    eprintln!();
    eprintln!("The subcommand may be omitted: action arguments imply 'write',");
    eprintln!("otherwise arguments are treated as 'read'.");
}

/// Repack action flags imply the `write` subprogram when no subcommand is given.
fn looks_like_write(args: &[String]) -> bool {
    const WRITE_FLAGS: &[&str] = &[
        "--add", "-A", "--cp", "-C", "--mv", "-M", "--rm", "-R", "-r",
        "--set-mode", "--set-owner", "--set-context", "--sparse-output",
        "--shared-blocks", "--shader-blocks", "--compact", "--compress",
        "--reserve-mb", "--reserve", "--convert-to", "--input", "--output",
    ];
    args.iter()
        .any(|a| WRITE_FLAGS.contains(&a.as_str()))
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let rest = &argv[1..];
    let result = match rest.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print_usage();
            Ok(())
        }
        Some("read") => read::run(&rest[1..]).map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
        Some("write") => write::run(&rest[1..]),
        Some(_) if looks_like_write(rest) => write::run(rest),
        Some(_) => read::run(rest).map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
