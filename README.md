# image-worker

> High-performance modular user-space Rust utility for deep inspection (`read`), modification (`write`), and mutual conversion (`convert`) of Android filesystem images (`ext4`, `EROFS`), as well as Android Sparse containers (`simg`).

The utility is completely standalone: it requires no superuser privileges (`root`), kernel mounts (`mount` / `loop` devices), FUSE, or external host binaries (`e2fsprogs`, `erofs-utils`, `simg2img`). All low-level block and metadata manipulations are performed strictly within the isolated process memory space.

---

## Key Features

* **100% Standalone User-Space**: Direct parsing, mutation, and generation of filesystem structures in pure safe Rust.
* **Zero-Panic Policy**: No `unwrap()`, `expect()`, or `panic!` across all runtime code paths. All binary structures are bounds-checked via the `crate::core::buf` module returning `Result<_, Error>`.
* **Strict Memory Discipline ($O(1)$ RAM / Streaming)**: Reading, writing, and conversions stream in fixed 64 KiB chunks for filesystems and 256 KiB chunks for Sparse containers. Peak resident memory (Peak RSS) stays within 5–25 MB even on multi-gigabyte images. Multi-gigabyte partitions never sit in RAM whole.
* **Flawless SELinux Preservation (Zero-Loss)**:
  * **ext4**: Extracts and serializes `security.selinux` labels from both inode inline storage (`extra area`) and external extended attribute blocks (`i_file_acl`).
  * **EROFS**: Parses both inline and shared xattr tables; generates deduplicated global `xattr_blkaddr` tables upon export while safely avoiding superblock collision during NID assignment.
  * **Seamless Roundtrip**: Bit-for-bit preservation of security contexts across `ext4 -> EROFS -> ext4` pipelines passing all `e2fsck` passes.
* **Full-Featured Deletion (`--rm` / `-R`)**: Safely removes files and directory subtrees, updating directory records and reclaiming block space.
* **True ext4 Shrinking (`--compact`)**: Bottom-up geometry calculation starting from 1 block group, factoring in block deduplication estimates (`--shared-blocks`).
* **Multithreaded EROFS Compressor**: Parallel cluster packer (LZ4 / Deflate) powered by `rayon`, automatically preventing incompressible data expansion.
* **EROFS Overlay Delta-Rebuild**: Near-instant metadata patching and file appending without decompressing existing compressed clusters (1.00x growth ratio).

---

## Repository Architecture

```text
src/main.rs        Entry point, command dispatch, and read/write auto-detection
src/core/          Unified low-level I/O core:
  buf.rs           Bounds-checked little-endian readers (u16le, u32le, u64le, bytes)
  error.rs         Unified error definitions (thiserror)
  image.rs         Raw / Android Sparse containers, stdin spooling, streaming simg encoder
  util.rs          File signature sniffer (ELF, APK, DEX), permission formatters, glob matchers
src/fs/            Filesystem backends (single source of truth for read and write):
  ext4/            Superblock, inodes, extent trees, directories, inline & block xattrs
  erofs/           Superblock, inodes, compact clusters, LZ4/Deflate/ZSTD, xattr tables
src/read/          `read` subcommand (auditing, search, metadata inspection, streaming cat)
src/write/         `write` subcommand and converters:
  mod.rs           Node tree, action queue, overlay/append/legacy EROFS rebuild engines
  ext4.rs          ext4 copy-on-write builder, block-group planner, compact & shared-blocks
  convert.rs       Pure streaming format converters without filesystem tree mutation
```

---

## Technical Highlights & Fixes Over Common Implementations

`image-worker` fixes multiple pervasive flaws found in standard Android image scripts and parsers:

| Spec / Format Quirk | Common Implementation Flaw | `image-worker` Implementation |
|---|---|---|
| **ext4 SELinux context** | Reading 4 bytes too early, stripping the `:s0` suffix. | Calculated strictly relative to `header + 4`; extracts and serializes full contexts. |
| **ext4 64bit detection** | Flags incorrectly read from `ro_compat` (offset 100). | Read strictly from `incompat` (offset 96) as mandated by the ext4 specification. |
| **EROFS xattr listing** | Parsing limited to inline entries; shared tables ignored. | Full traversal of both inline and shared xattr tables without losing security labels. |
| **EROFS Directory padding** | Trailing padding bytes (`\0`) leak into filenames, breaking glob matching. | Trailing padding NULs are safely stripped during directory record parsing. |
| **EROFS 64-bit UID/GID** | Extended inodes treated UID as u16 with misaligned offsets (`65537000:0`). | UID/GID are read and written strictly as `u32` at byte offsets 24..32. |
| **ext4 Block Aliasing** | Repackers alias old block offsets, leading to silent data corruption. | File payloads stream exclusively into freshly allocated physical blocks. |
| **ext4 Sparse Output** | Requests for sparse output containers are silently ignored. | Native streaming Android Sparse encoder fully integrated for ext4 targets. |
| **ext4 Shrink Lock** | Group count clamped to `.max(groups)`, preventing shrinking. | Bottom-up fixed-point algorithm starting from 1 group; removals physically shrink raw images. |

---

## Hard Integrity Caps

To prevent denial-of-service, unbounded stack recursion, and OOM panics on corrupted or malicious images, strict caps are enforced:

* **Maximum single file size**: 16 GiB.
* **Maximum decompressed cluster (pcluster)**: 64 MiB (standard clusters are $\le$ 512 KiB).
* **Maximum inline xattr area**: 1 MiB.
* **Maximum directory recursion depth**: 512 levels (protects against cyclic references).
* **Maximum ext4 extent tree depth**: 16 levels.
* **Deduplication set cap**: Hash set capped at 2,000,000 unique blocks (~16 MB RAM) to prevent unbounded memory growth during deduplication planning.

---

## Building & Compilation

**Prerequisites**:
* Rust toolchain (MSRV 1.85+ or stable release edition 2024).
* Cargo package manager.
* For static builds: musl toolchains or `cross` (see `build.sh --help` for per-distro install hints).

```bash
# Clone the repository
git clone https://github.com/leegarchat/image-worker.git
cd image-worker

# Fast local build (host target, dynamic)
cargo build --release
# -> target/release/image-worker

# Static multi-arch builds via build.sh (musl, stripped):
#   --cargo | --cross | --auto   build method (auto = cross if containers exist, else cargo)
#   --arch all|x64|x86|arm64|arm32
./build.sh --cargo --arch x64      # x86_64-unknown-linux-musl
./build.sh --cross --arch arm64    # aarch64 via containers
./build.sh --arch all              # x86_64, x86, aarch64, armv7

# Run comprehensive test suite
cargo test
```

Build outputs:

* `dist/` — static binaries `image-worker-linux-*` (x86_64, x86, arm64, arm32).
* `target/push/` — push-ready copies `{name}_{arch}` (`x64`, `x86`, `arm64`, `arm32`), refreshed per built arch, e.g. for devices:
  `adb push target/push/image-worker_arm64 /data/local/` (both `dist/` and `target/` are gitignored).

---

## CLI Reference

`image-worker` automatically determines the subprogram: file modification flags switch the engine to `write`, while read-only flags default to `read`:

```text
image-worker read [OPTIONS] <image|-> [inner_path]
image-worker write <input> <output> ACTION... [OPTIONS]
image-worker write <input> <output> --convert-to <erofs|ext4> [OPTIONS]
```

Use `-` as a path argument to read from `stdin` or stream to `stdout`.

### 1. `read` Subprogram (Inspection & Auditing)

* `-i, --info` — Superblock diagnostics, container format, and filesystem feature flags.
* `-l, --ls` — Directory listing (defaults to root `/`).
* `-f, --find [pattern]` — Recursive search supporting glob wildcards (`*`, `?`).
* `-c, --cat` — Stream raw file bytes directly to `stdout` ($O(1)$ RAM).
* `-t, --type` — Display node type (`file`, `directory`, `symlink`).
* `-F, --file-type` — Detect content type via magic signature (ELF, APK, DEX, XML, text).
* `-o, --owner` — Display `UID:GID`.
* `-p, --permissions` — Symbolic permission bits (`rwxr-xr-x`).
* `-n, --numeric-permissions` — Octal permission bits (`0755`).
* `-s, --size` — Size in bytes.
* `-H, --human-size` — Human-readable size formatting (`KiB`, `MiB`, `GiB`).
* `-Z, --context` — Display SELinux security context string.
* `--stdin` — Read input image from standard input.

**Examples:**
```bash
# Inspect container format and filesystem superblock
image-worker read --info vendor.img

# List system directory entries with permissions, owners, and SELinux contexts
image-worker read -l -p -o -Z vendor.img /etc

# Recursively locate fstab configuration files
image-worker read --find="*fstab*" -Z vendor.img /

# Stream a build.prop file to stdout
image-worker read --cat vendor.img /etc/build.prop > build.prop
```

---

### 2. `write` Subprogram (Modification, Deletion & Repacking)

**File Operations:**
* `--add, -A <host_path> <image_path>` — Add or replace a file (`-` reads payload from stdin).
* `--cp, -C <src_path> <dst_path>` — Copy within the image with block-level deduplication.
* `--mv, -M <src_path> <dst_path>` — Rename/move node while retaining inode identity.
* `--rm <image_path>` — Delete a single file or empty directory.
* `-R, -r <image_path>` — Recursively delete a directory and its entire subtree.

**Action Modifiers (passed immediately after `--add`/`--cp`/`--mv`):**
* `--mode <octal_mode>` — Set permission mode (e.g., `0644`, `0755`).
* `--uid <UID>` / `--gid <GID>` — Set numeric ownership.
* `--context <selinux_context>` — Explicitly assign a SELinux context.

**In-Place Metadata Mutations:**
* `--set-mode <path> <mode>` — Change permission bits.
* `--set-owner <path> <uid:gid>` — Change owner credentials.
* `--set-context <path> <context>` — Update SELinux label.

**Geometry & Size Optimization Options:**
* `--compact` — Shrink ext4 raw image to minimal block-group count (implied by `--rm`).
* `--shared-blocks <y|n>` — Deduplicate duplicate data blocks in ext4 (zeros become sparse holes).
* `--reserve-mb <size>` — Reserve guaranteed free space in ext4 output (`64`, `128M`, `1G`).
* `--compress <lz4|deflate|none>` — Select compression algorithm for EROFS.
* `--sparse-output` — Output as an Android Sparse container (`simg`).

**Examples:**
```bash
# Replace system file and assign correct SELinux context
image-worker write vendor.img out.img \
    --add hosts /etc/hosts --mode 0644 --context u:object_r:vendor_configs_file:s0

# Stream modifications on the fly through a pipe
cat new_fstab | image-worker write vendor.img out.img \
    --add - /etc/fstab.qcom --mode 0644

# Recursively remove an application directory and shrink the raw ext4 partition
image-worker write vendor.img out.img -R /app/PrebuiltApp --compact

# Rebuild an EROFS partition using multithreaded Deflate compression
image-worker write vendor_erofs.img out_deflate.img \
    --rm /etc/unneeded.xml --compress deflate
```

---

### 3. `convert` Subprogram (Streamed Cross-Format Conversion)

```text
image-worker write <input> <output> --convert-to <erofs|ext4> [OPTIONS]
```

* `ext4 -> erofs` — Encodes tree into EROFS. Uses LZ4 by default (`--compress lz4`). SELinux contexts are packed into an optimized shared table.
* `erofs -> ext4` — Reconstructs ext4 extent trees from EROFS structures. Symlinks, permissions, ownerships, and contexts transfer byte-for-byte. Output passes `e2fsck -fn` cleanly.
* `raw <-> sparse` (Same FS) — Streams container transformation without repacking the underlying filesystem.

**Examples:**
```bash
# Convert ext4 partition to EROFS with default LZ4 compression
image-worker write vendor_ext4.img vendor_erofs.img --convert-to erofs

# Convert ext4 to high-density EROFS with Deflate compression
image-worker write vendor_ext4.img vendor_erofs.img --convert-to erofs --compress deflate

# Convert EROFS to ext4 with 128 MB reserved headroom
image-worker write vendor_erofs.img vendor_ext4.img --convert-to ext4 --reserve-mb 128

# Expand sparse image container to raw block image
image-worker write vendor.sparse.img vendor.raw.img --convert-to erofs

# Compress raw block image to an Android Sparse container
image-worker write vendor.raw.img vendor.sparse.img --convert-to ext4 --sparse-output
```

---

## Critical Environment Notice: `TMPDIR` Exhaustion

On modern Linux distributions (Ubuntu, Debian, Fedora, Arch), `/tmp` is mounted by default as a **RAM-backed virtual disk (`tmpfs`)**, constrained to 50% of physical RAM capacity.

### The Failure Mode
When performing operations on large partitions (1.5–4.0 GB), streaming through `stdin`/`stdout` (`-`), or producing sparse output, `image-worker` spools intermediate bytes to a temporary seekable file (`spool_temp`). 
On a machine with 16 GB RAM, `/tmp` holds at most 8 GB. Processing multiple large system partitions in sequence will rapidly exhaust `tmpfs` space. The kernel then fails filesystem operations with `ENOSPC (No space left on device)`, which can result in truncated or zero-byte output images.

### Mitigation
`image-worker` strictly respects the POSIX standard and checks the `TMPDIR` environment variable before falling back to `/tmp`. When processing large disk images, always point your temporary directory to a physical drive:

```bash
# Single command execution on a physical NVMe/SSD partition:
TMPDIR=/mnt/nvme_disk/temp image-worker write vendor.img out.img --convert-to erofs

# Export for an entire shell session or pipeline script:
export TMPDIR=~/build_workspace/tmp
```

---

## Performance Benchmarks & Validation

Benchmarks recorded on the reference test system:
* **CPU**: AMD Ryzen 7 8845HS (8 cores / 16 threads, Rayon MT).
* **OS**: Ubuntu 26.04 LTS (Linux kernel 7.0).
* **Build profile**: `cargo build --release`.

Test results on vendor Android partitions:

| Scenario | Input Image | Elapsed | Peak RSS | Output Size | Integrity Status |
|---|---|---|---|---|---|
| **READ**: Metadata audit & parsing | 14 fixtures (ext4/EROFS) | < 0.05 s | 2.6 MB | — | SELinux OK, 100% PASS |
| **WRITE**: EROFS single-add | `plain.img` (2.4 GB) | 3.91 s | 5.6 MB | 2.4 GB | Tail appended, PASS |
| **WRITE**: EROFS overlay delta | `plain.img` (2.4 GB) | 7.64 s | 5.9 MB | 2.4 GB | No bloat (1.00x) |
| **WRITE**: ext4 rebuild + growth | `shiba_vendor` (850 MB) | 2.78 s | 5.5 MB | 1024 MB | `e2fsck -fn`: clean (5/5) |
| **WRITE**: ext4 remove -R (shrink) | `shiba_vendor` (850 MB) | 1.83 s | 4.8 MB | **768 MB** | Space reclaimed, clean |
| **WRITE**: ext4 shared+compact | `shiba_shared` (848 MB) | 4.14 s | 20.9 MB | 896 MB | Hash deduplicated, clean |
| **CONVERT**: EROFS -> ext4 | `marble_erofs` (1.5 GB) | 11.07 s | 9.9 MB | 2.82 GB | SELinux OK, e2fsck clean |
| **CONVERT**: ext4 -> EROFS plain | `shiba_vendor` (850 MB) | 2.34 s | 7.8 MB | 898.6 MB | All-plain EROFS |
| **CONVERT**: ext4 -> EROFS LZ4 | `shiba_vendor` (850 MB) | **2.63 s** | 7.7 MB | **766.9 MB** | CPU 312%, MD5 match |
| **CONVERT**: ext4 -> EROFS Deflate | `shiba_vendor` (850 MB) | **6.04 s** | 12.6 MB | **691.8 MB** | CPU 770%, MD5 match |
| **ROUNDTRIP**: ext4 -> erofs -> ext4 | `shiba_vendor` (850 MB) | 4.07 s | 10.1 MB | Original | 3076 files MD5 match |

---

## Licensing

`image-worker` is distributed under a dual license model (**Dual-licensed**):

* **Apache License, Version 2.0** (`LICENSE-APACHE` or http://www.apache.org/licenses/LICENSE-2.0)
* **MIT License** (`LICENSE-MIT` or http://opensource.org/licenses/MIT)

You may freely use, modify, and distribute this software under either license at your option.