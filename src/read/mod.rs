//! `read` subcommand — byte-compatible with `image-inspect`.
//!
//! Invoked as `image-worker read [OPTIONS] <image|-> [path]`.
//! CLI, output format and traversal semantics mirror image-inspect exactly
//! (auto-swap of image/path, `--stdin`, `key=value` metadata fields,
//! `.`/`..` filtering, streaming `cat`).

use crate::core::{Error, Image, util};
use crate::fs::{erofs, ext4};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
enum Operation {
    List,
    Find(Option<String>),
    Cat,
}

#[derive(Debug, Clone, Copy, Default)]
struct MetadataOptions {
    file_type: bool,
    content_type: bool,
    owner: bool,
    permissions: bool,
    numeric_permissions: bool,
    size: bool,
    human_size: bool,
    context: bool,
}

impl MetadataOptions {
    fn requested(self) -> bool {
        self.file_type
            || self.content_type
            || self.owner
            || self.permissions
            || self.numeric_permissions
            || self.size
            || self.human_size
            || self.context
    }
}

#[derive(Debug)]
struct Cli {
    image: PathBuf,
    path: String,
    info: bool,
    operation: Option<Operation>,
    metadata: MetadataOptions,
}

pub fn run(args: &[String]) -> Result<(), Error> {
    let Some(cli) = parse_cli(args)? else {
        print_help();
        return Ok(());
    };
    run_cli(&cli)
}

fn run_cli(cli: &Cli) -> Result<(), Error> {
    let mut image = Image::open(&cli.image)?;
    // Single shared probe (see crate::fs): EROFS first, then ext4.
    match crate::fs::probe_fs(&mut image)? {
        crate::fs::FsKind::Erofs => {
            let fs = erofs::Superblock::read(&mut image)?;
            return run_erofs(&mut image, &fs, cli);
        }
        crate::fs::FsKind::Ext4 => {
            let sb = ext4::Superblock::read(&mut image)?;
            return run_ext4(&mut image, &sb, cli);
        }
    }
}

// ── ext4 ─────────────────────────────────────────────────────────────

fn run_ext4(image: &mut Image, sb: &ext4::Superblock, cli: &Cli) -> Result<(), Error> {
    if matches!(cli.operation, Some(Operation::Cat)) && (cli.info || cli.metadata.requested()) {
        return Err(Error::invalid(
            "--cat cannot be combined with info or metadata output",
        ));
    }
    if cli.info {
        print_ext4_info(image, sb);
    }
    match cli
        .operation
        .clone()
        .or_else(|| (!cli.info).then_some(Operation::List))
    {
        Some(Operation::List) => {
            let (inode_number, inode) = find_path_ext4(image, sb, &cli.path)?;
            if inode.mode & ext4::EXT4_DIRECTORY == 0 {
                return Err(Error::invalid(format!("{} is not a directory", cli.path)));
            }
            for entry in ext4::dir::read(sb, image, inode_number)? {
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                if cli.metadata.requested() {
                    let (dent, entry_inode) = ext4::dir::stat(sb, image, &entry)?;
                    print_ext4_entry(&dent, &entry_inode, image, sb, cli.metadata)?;
                } else {
                    let marker = if entry.file_type == 2 { "/" } else { "" };
                    println!("{}{marker}", entry.name);
                }
            }
        }
        Some(Operation::Find(pattern)) => {
            let (inode_number, inode) = find_path_ext4(image, sb, &cli.path)?;
            if inode.mode & ext4::EXT4_DIRECTORY == 0 {
                return Err(Error::invalid(format!("{} is not a directory", cli.path)));
            }
            let mut pending = vec![(inode_number, cli.path.clone())];
            while let Some((dir_inode, dir_path)) = pending.pop() {
                for entry in ext4::dir::read(sb, image, dir_inode)? {
                    if entry.name == "." || entry.name == ".." {
                        continue;
                    }
                    let entry_path = image_path(&dir_path, &entry.name);
                    if pattern
                        .as_deref()
                        .is_none_or(|p| util::matches_pattern(&entry.name, p))
                    {
                        if cli.metadata.requested() {
                            let (mut dent, entry_inode) =
                                ext4::dir::stat(sb, image, &entry)?;
                            dent.name = entry_path.clone();
                            print_ext4_entry(
                                &dent,
                                &entry_inode,
                                image,
                                sb,
                                cli.metadata,
                            )?;
                        } else {
                            println!("{entry_path}");
                        }
                    }
                    if entry.file_type == 2 {
                        pending.push((entry.inode, entry_path));
                    }
                }
            }
        }
        Some(Operation::Cat) => {
            let (_, inode) = find_path_ext4(image, sb, &cli.path)?;
            if inode.mode & ext4::EXT4_DIRECTORY != 0 {
                return Err(Error::invalid(format!("{} is a directory", cli.path)));
            }
            let stdout = io::stdout();
            let mut output = stdout.lock();
            ext4::inode::write_data(sb, image, &inode, &mut output)?;
            output.flush()?;
        }
        None => {}
    }
    Ok(())
}

fn find_path_ext4(
    image: &mut Image,
    sb: &ext4::Superblock,
    path: &str,
) -> Result<(u32, ext4::inode::Ext4Inode), Error> {
    let mut inode_number = ext4::EXT4_ROOT_INODE;
    for component in path.split('/').filter(|c| !c.is_empty()) {
        let entry = ext4::dir::read(sb, image, inode_number)?
            .into_iter()
            .find(|e| e.name == component)
            .ok_or_else(|| Error::invalid(format!("path component not found: {component}")))?;
        inode_number = entry.inode;
    }
    Ok((inode_number, ext4::inode::read(sb, image, inode_number)?))
}

fn print_ext4_info(image: &Image, sb: &ext4::Superblock) {
    println!("format={}", image.container_name());
    println!("size={}", image.size());
    println!("filesystem=ext4");
    println!("block_size={}", sb.block_size);
    println!("inode_size={}", sb.inode_size);
    println!("block_count={}", sb.block_count);
    println!("feature_compat=0x{:08x}", sb.compat);
    println!("feature_incompat=0x{:08x}", sb.incompat);
    println!("feature_ro_compat=0x{:08x}", sb.ro_compat);
    println!(
        "feature_extents={}",
        sb.incompat & ext4::EXT4_EXTENTS != 0
    );
    println!("feature_64bit={}", sb.incompat & ext4::EXT4_64BIT != 0);
    println!(
        "feature_metadata_csum={}",
        sb.ro_compat & ext4::EXT4_METADATA_CSUM != 0
    );
    println!(
        "feature_shared_blocks={}",
        sb.ro_compat & ext4::EXT4_SHARED_BLOCKS != 0
    );
    if let Some((block_size, chunks)) = image.sparse_info() {
        println!("sparse_block_size={block_size}\nsparse_chunks={chunks}");
    }
}

fn print_ext4_entry(
    entry: &crate::fs::DirEntry,
    inode: &ext4::inode::Ext4Inode,
    image: &mut Image,
    sb: &ext4::Superblock,
    options: MetadataOptions,
) -> Result<(), Error> {
    if !options.requested() {
        println!("{}", entry.name);
        return Ok(());
    }
    let mut fields = Vec::new();
    if options.file_type {
        fields.push(format!("type={}", util::file_type(entry.mode)));
    }
    if options.content_type {
        let sample = ext4::inode::read_prefix(sb, image, inode, util::sample_size() as u64)?;
        fields.push(format!(
            "content_type={}",
            util::detect_content_type(&sample)
        ));
    }
    if options.owner {
        fields.push(format!("owner={}:{}", entry.uid, entry.gid));
    }
    if options.permissions {
        fields.push(format!(
            "permissions={}",
            util::symbolic_permissions(entry.mode)
        ));
    }
    if options.numeric_permissions {
        fields.push(format!("mode={}", util::numeric_permissions(entry.mode)));
    }
    if options.size {
        fields.push(format!("size={}", entry.size));
    }
    if options.human_size {
        fields.push(format!("human_size={}", util::human_size(entry.size)));
    }
    if options.context {
        fields.push(format!(
            "context={}",
            entry.context.as_deref().unwrap_or("-")
        ));
    }
    fields.push(entry.name.clone());
    println!("{}", fields.join(" "));
    Ok(())
}

// ── EROFS ────────────────────────────────────────────────────────────

fn run_erofs(image: &mut Image, fs: &erofs::Superblock, cli: &Cli) -> Result<(), Error> {
    if matches!(cli.operation, Some(Operation::Cat)) && (cli.info || cli.metadata.requested()) {
        return Err(Error::invalid(
            "--cat cannot be combined with info or metadata output",
        ));
    }
    if cli.info {
        println!(
            "format={}\nfilesystem=erofs\nblock_size={}\nroot_nid={}\nfeature_incompat=0x{:08x}",
            image.container_name(),
            fs.block_size,
            fs.root_nid,
            fs.feature_incompat
        );
    }
    match cli
        .operation
        .clone()
        .or_else(|| (!cli.info).then_some(Operation::List))
    {
        Some(Operation::List) => {
            let inode = resolve_erofs(image, fs, &cli.path)?;
            for entry in erofs::dir::read(fs, image, inode.nid)? {
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                if cli.metadata.requested() {
                    let (dent, dent_inode) =
                        erofs::dir::stat(fs, image, &entry, cli.metadata.context)?;
                    print_erofs_entry(image, fs, &dent, &dent_inode, cli.metadata)?;
                } else {
                    println!(
                        "{}{}",
                        entry.name,
                        if entry.file_type == 2 { "/" } else { "" }
                    );
                }
            }
        }
        Some(Operation::Find(pattern)) => {
            let root = resolve_erofs(image, fs, &cli.path)?;
            let mut pending = vec![(root.nid, cli.path.clone())];
            while let Some((nid, parent)) = pending.pop() {
                let inode = erofs::inode::read(fs, image, nid)?;
                for entry in erofs::dir::read(fs, image, inode.nid)? {
                    if entry.name == "." || entry.name == ".." {
                        continue;
                    }
                    let path = image_path(&parent, &entry.name);
                    if pattern
                        .as_deref()
                        .is_none_or(|p| util::matches_pattern(&entry.name, p))
                    {
                        if cli.metadata.requested() {
                            let (mut dent, dent_inode) =
                                erofs::dir::stat(fs, image, &entry, cli.metadata.context)?;
                            dent.name = path.clone();
                            print_erofs_entry(image, fs, &dent, &dent_inode, cli.metadata)?;
                        } else {
                            println!("{path}");
                        }
                    }
                    if entry.file_type == 2 {
                        pending.push((entry.nid, path));
                    }
                }
            }
        }
        Some(Operation::Cat) => {
            let inode = resolve_erofs(image, fs, &cli.path)?;
            if inode.mode & erofs::DIRECTORY != 0 {
                return Err(Error::invalid(format!("{} is a directory", cli.path)));
            }
            // Streaming: multi-GB files never sit in RAM whole.
            erofs::inode::read_data_streaming(fs, image, &inode, &mut io::stdout().lock())?;
        }
        None => {}
    }
    Ok(())
}

fn resolve_erofs(
    image: &mut Image,
    fs: &erofs::Superblock,
    path: &str,
) -> Result<erofs::inode::Inode, Error> {
    let mut inode = erofs::inode::read(fs, image, fs.root_nid)?;
    for component in path.split('/').filter(|p| !p.is_empty()) {
        let entry = erofs::dir::read(fs, image, inode.nid)?
            .into_iter()
            .find(|e| e.name == component)
            .ok_or_else(|| Error::invalid(format!("path component not found: {component}")))?;
        inode = erofs::inode::read(fs, image, entry.nid)?;
    }
    Ok(inode)
}

fn print_erofs_entry(
    image: &mut Image,
    fs: &erofs::Superblock,
    entry: &crate::fs::DirEntry,
    inode: &erofs::inode::Inode,
    options: MetadataOptions,
) -> Result<(), Error> {
    if !options.requested() {
        println!(
            "{}{}",
            entry.name,
            if entry.file_type == 2 { "/" } else { "" }
        );
        return Ok(());
    }
    let mut fields = Vec::new();
    if options.file_type {
        fields.push(format!("type={}", util::file_type(entry.mode)));
    }
    if options.content_type {
        // Bounded sniff: only the leading sample is decoded, never the
        // whole file (matters for multi-GB compressed payloads).
        let sample =
            erofs::inode::read_prefix(fs, image, &inode, util::sample_size() as u64)?;
        fields.push(format!(
            "content_type={}",
            util::detect_content_type(&sample)
        ));
    }
    if options.owner {
        fields.push(format!("owner={}:{}", entry.uid, entry.gid));
    }
    if options.permissions {
        fields.push(format!(
            "permissions={}",
            util::symbolic_permissions(entry.mode)
        ));
    }
    if options.numeric_permissions {
        fields.push(format!("mode={}", util::numeric_permissions(entry.mode)));
    }
    if options.size {
        fields.push(format!("size={}", entry.size));
    }
    if options.human_size {
        fields.push(format!("human_size={}", util::human_size(entry.size)));
    }
    if options.context {
        fields.push(format!(
            "context={}",
            entry.context.as_deref().unwrap_or("-")
        ));
    }
    fields.push(entry.name.clone());
    println!("{}", fields.join(" "));
    Ok(())
}

// ── CLI (image-inspect compatible) ───────────────────────────────────

fn print_help() {
    println!(
        "image-inspect - inspect Android filesystem images without mounting\n\n\
    Usage:\n  image-worker read [OPTIONS] <image|-> [path]\n\n\
        Options:\n  -i, --info                print image and ext4 diagnostic information\n  -l, --ls                  list entries at path\n  -f, --find [pattern]      recursively find all entries or match names\n  -c, --cat                 write selected file bytes to stdout\n  -t, --type                show entry type\n  -F, --file-type           detect content type from file signature\n  -o, --owner               show numeric UID:GID\n  -p, --permissions         show symbolic permissions\n  -n, --numeric-permissions show octal permissions\n  -s, --size                show size in bytes\n  -H, --human-size          show human-readable size\n  -Z, --context             show SELinux context when present\n  -h, --help                print this help message\n\n\
    Examples:\n  image-worker read vendor.img /etc\n  image-worker read --info --ls --type vendor.img /etc\n  image-worker read --find fstab.zuma vendor.img /\n  image-worker read --find --type --permissions vendor.img /"
    );
    println!(
        "\n  --stdin reads the image from standard input.\n  Example: cat vendor.img | image-worker read --stdin /etc/fstab.qcom"
    );
}

fn parse_cli(args: &[String]) -> Result<Option<Cli>, Error> {
    let mut positional: Vec<String> = Vec::new();
    let mut info = false;
    let mut stdin = false;
    let mut operation: Option<Operation> = None;
    let mut metadata = MetadataOptions::default();
    for argument in args {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "-i" | "--info" => info = true,
            "--stdin" => stdin = true,
            "-t" | "--type" => metadata.file_type = true,
            "-F" | "--file-type" => metadata.content_type = true,
            "-o" | "--owner" => metadata.owner = true,
            "-p" | "--permissions" => metadata.permissions = true,
            "-n" | "--numeric-permissions" => metadata.numeric_permissions = true,
            "-s" | "--size" => metadata.size = true,
            "-H" | "--human-size" => metadata.human_size = true,
            "-Z" | "--context" => metadata.context = true,
            "-l" | "--ls" => {
                if operation.is_some() {
                    return Err(Error::invalid("only one operation may be selected"));
                }
                operation = Some(Operation::List);
            }
            "-c" | "--cat" => {
                if operation.is_some() {
                    return Err(Error::invalid("only one operation may be selected"));
                }
                operation = Some(Operation::Cat);
            }
            "-f" | "--find" => {
                if operation.is_some() {
                    return Err(Error::invalid("only one operation may be selected"));
                }
                operation = Some(Operation::Find(None));
            }
            value if value.starts_with("--find=") => {
                let pattern = value.trim_start_matches("--find=");
                if pattern.is_empty() {
                    return Err(Error::invalid("--find pattern cannot be empty"));
                }
                if operation.is_some() {
                    return Err(Error::invalid("only one operation may be selected"));
                }
                operation = Some(Operation::Find(Some(pattern.into())));
            }
            "-" => positional.push(argument.clone()),
            _ if argument.starts_with('-') => {
                return Err(Error::invalid(format!("unknown option: {argument}")));
            }
            _ => positional.push(argument.clone()),
        }
    }
    let (image, path, operation) = if stdin {
        match operation {
            Some(Operation::Find(None)) if !positional.is_empty() => (
                Some("-".to_owned()),
                positional.get(1).cloned(),
                Some(Operation::Find(Some(positional[0].clone()))),
            ),
            operation => (Some("-".to_owned()), positional.first().cloned(), operation),
        }
    } else {
        match operation {
            Some(Operation::Find(None))
                if positional.len() >= 2 && !Path::new(&positional[0]).is_file() =>
            {
                (
                    positional.get(1).cloned(),
                    positional.get(2).cloned(),
                    Some(Operation::Find(Some(positional[0].clone()))),
                )
            }
            operation => {
                if positional.len() >= 2 {
                    let first = &positional[0];
                    let second = &positional[1];
                    if !Path::new(first).is_file() && Path::new(second).is_file() {
                        (
                            Some(second.clone()),
                            Some(first.clone()),
                            operation,
                        )
                    } else {
                        (
                            positional.first().cloned(),
                            positional.get(1).cloned(),
                            operation,
                        )
                    }
                } else {
                    (
                        positional.first().cloned(),
                        positional.get(1).cloned(),
                        operation,
                    )
                }
            }
        }
    };
    let image = image
        .ok_or_else(|| Error::invalid("image path is required; use --help for usage"))?;
    if !stdin && !Path::new(&image).is_file() {
        return Err(Error::invalid(format!("image file not found: {image}")));
    }
    Ok(Some(Cli {
        image: PathBuf::from(image),
        path: path.unwrap_or_else(|| "/".into()),
        info,
        operation,
        metadata,
    }))
}

fn image_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("/{}/{}", parent.trim_matches('/'), name)
    }
}
