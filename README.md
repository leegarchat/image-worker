# image-worker-rework

Single-core, modular successor of `image-inspect` (read) and
`image-repack-rework` (write). Existing tools were **not** modified;
this crate re-uses their logic on one shared core. No kernel mounts,
no external helpers at runtime — everything works inside the process,
exactly like the `read` subprogram does.

```text
image-worker-rework read [OPTIONS] <image|-> [path]
image-worker-rework write <input> <output> ACTION... [OPTIONS]
image-worker-rework write <input> <output> --convert-to <erofs|ext4> [OPTIONS]
```

The subcommand may be omitted: arguments with repack actions
(`--add/--cp/--mv/--set-*/--sparse-output/--shared-blocks/--input/--output`)
are treated as `write`, everything else as `read`
(drop-in compatible with both original CLIs).

`write` handles every `images/*.img` (except `super*`) plus all of
`images/erofs-fixtures/` (plain/lz4/lz4hc-8k/deflate, raw and sparse).

## Layout

```text
src/main.rs        dispatch + subcommand-less compat heuristic
src/core/          THE single core (both subprograms use only this for I/O)
  error.rs         unified Error (thiserror)
  image.rs         Image container: raw / Android sparse (strict validation),
                   stdin spool ("-"), temp-file helper, in-RAM sparse encoder
                   plus streaming file->file / file->stdout sparse encoders
                   (O(1) RAM, two passes)
  util.rs          file_type / permissions / human_size / matches_pattern /
                   full signature detection + sample_size + unit tests
src/fs/            read-side filesystem backends over core::Image
  ext4/            Superblock/probe (mod), inode+extents+streaming (inode),
                   directory (dir), SELinux xattr (selinux)
  erofs/           Superblock/probe (mod), inode+plain/inline layouts (inode),
                   directory parser (dir), compact clusters + LZ4/DEFLATE/ZSTD
                   (compress)
src/read/          `read` subcommand: image-inspect CLI and output, byte-compatible
                   (format/size/filesystem/feature_* / sparse_* fields,
                   `key=value` metadata, `.`/`..` filtering, streaming cat,
                   --stdin, image/path auto-swap, guards)
src/write/         `write` subcommand: repack engine on the core
  mod.rs           Node tree, Action/Metadata queue, CLI, EROFS append +
                   overlay delta-rebuild + legacy rebuild, sparse writers
                   (ported; private Reader replaced by core::Image)
  ext4.rs          ext4 copy-on-write rebuild with on-demand growth,
                   real --sparse-output, --reserve-mb and "-" stdout
  convert.rs       pure format converters (no file actions)
```

## `write` usage notes

- Replace (`--add` onto an existing path) keeps mode/uid/gid/context
  unless `--mode/--uid/--gid/--context` say otherwise. ext4 SELinux
  contexts are serialized inline (extra area, kernel/e2fsck-exact
  layout); EROFS contexts of untouched files survive in place via the
  overlay; explicit contexts route through the full rebuild (shared
  table), never dropped, never an error. `--set-context` works for
  both filesystems.
- `--reserve-mb N` (also `--reserve N`, K/M/G suffixes, bare = MiB)
  guarantees at least N MiB free in the rebuilt ext4 image. Without it
  the output is packed with one block-group margin. Never shrinks.
- `--convert-to erofs|ext4` converts without file actions (actions are
  rejected in this mode):
  same fs -> container-only streaming conversion (payload untouched);
  erofs->ext4 -> fresh ext4 rebuild (names/sizes/modes/uid/gid/contexts
  exact, dirs get fresh sizes);
  ext4->erofs  -> fresh all-plain EROFS (inputs stream through 64 KiB
  quanta; contexts pack into a shared xattr table, special files are
  skipped with a warning). `--sparse-output` selects the container for
  any target; `--shared-blocks`/`--reserve-mb` are ext4-target-only.
- Full format/compression matrix: `write --help`.

## Architecture: one shared parsing layer

All low-level parsing lives in `crate::fs` (single source of truth);
both `read` and the `write` engines (EROFS source, ext4 source,
converter adapters) call into it. Deleted in favor of it: the write
side's private EROFS superblock/inode/decompress/LZ4/directory code,
its ext4 superblock/inode/extent/directory code, both whole-image
sparse encoders, and both LE-reader helper sets.

`crate::core::buf` (`u16le`/`u32le`/`u64le`/`bytes`) returns clean
`Error::Invalid` on truncated input. There is zero `unwrap()`,
`expect()` and `panic!` in non-test code; corrupt images, absurd sizes
(16 GiB file cap, 64 MiB pcluster cap, 1 MiB xattr caps, 512-deep tree
cap, sparse sanity checks) all fail as errors, never aborts.

## How `write` works (and why it stays small in RAM/disk)

- EROFS single `--add` → append path (original blocks untouched, tail
  appended, streaming emit). Byte-identical to `image-repack-rework`.
- EROFS multi-action queues → **overlay delta-rebuild** (new): every
  untouched byte stays exactly where it is — compressed payloads, maps
  and indexes are never even read. Only changed/new directory blocks,
  patched inodes and appended file data are emitted, streaming, through
  `PatchedReader`. Peak RAM is bounded by the largest *touched* file
  (measured: 7 MB on a 1.3 GB deflate image vs 123 MB before; 2 s vs
  66 s; output same size instead of 2x). Plain files copied/moved with
  zero copying (shared source blocks); only copied *compressed* files
  are stored decompressed-plain.
  Anything the overlay cannot express (new host directories, directory
  copies) prints `overlay fallback: <reason>` to stderr and runs the
  legacy logical rebuild (which also handles explicit `--context` via a
  fresh shared table). Moves keep their inode identity
  (`insert_path(keep_identity)`); the legacy rebuild renumbers anyway,
  so its output is unaffected.
- ext4 → full copy-on-write rebuild, but grown on demand: vendor images
  are routinely 0% free, so demand is estimated up front, the filesystem
  is extended by whole block groups (never shrunk), with a deterministic
  grow-and-retry net. Measured: 3.4 s, 7 MB RAM on a 891 MB image.
- ext4 `--sparse-output` is honored (was silently ignored upstream):
  raw is built to a temp file, then streamed as a sparse container
  (sibling temp + rename for files, two-pass stream for stdout).
  ext4 output `-` streams raw or sparse to stdout via temp file.
- EROFS uid/gid bugfixes vs upstream: extended (64-byte) inodes carry
  uid/gid as u32 at bytes 24..32 (not 32..40, not u16). Fixed in the
  write-side inode reader, the non-dir tree reader, the metadata-only
  patcher and the overlay (which also preserves xattrs/link counts by
  copying the original inode and patching only data fields). Previously
  `--set-owner` on an extended inode wrote `65537000:0`-style garbage.

## Deviations from image-inspect (read side, intentional)

1. EROFS directory names are trimmed of trailing NUL padding
   (`media_profiles_*.xml` printed clean instead of with `\0` garbage).
   Padding bytes are not part of the name and break shell matching.
2. ext4 64bit detection reads the flag from `incompat` (offset 96);
   image-inspect reads `ro_compat` (offset 100).
3. EROFS `-Z` shows real contexts from shared/inline xattrs
   (image-inspect always printed `context=-` there).
4. ext4 `-Z` shows full NUL-terminated values (`...:s0`), verified
   against debugfs; image-inspect reads 4 bytes early (shifted window)
   and its display silently drops the `:s0` suffix.

## SELinux design (verified against e2fsprogs 1.47.2 sources)

- EROFS xattr layout (reverse-engineered, validated: exact area tiling
  and shared-entry resolution with zero failures over all fixtures):
  `[w0: u32 opaque][shared_count: u32][reserved: u32]` + shared IDs +
  inline `{len, index, size}+name+value` entries, 4-padded. Shared ID `k`
  lives at `xattr_blkaddr*block_size + k*4`. Writer emits `w0 = 0`
  (accepted by all observed images, including stock vendor ones).
- ext4 inline xattr: `e_value_offs` is relative to `header + 4` (not the
  header start), values packed backwards from the area end, entries
  chained by `NEXT = entry + LEN(name)` with an explicit zero
  terminator. An earlier revision used header-relative offsets and
  failed e2fsck with "allocation collision"; the current layout passes
  clean and matches debugfs byte-for-byte.
- EROFS outputs: fresh inodes with contexts reserve a 2nd 32-byte slot
  (32B inode + 16B header/ID area); NID assignment skips slots colliding
  with the superblock `[1024, 1152)` (mkfs does the same); unique
  contexts pack into a shared table referenced by `xattr_blkaddr`.
- No silent drops: explicit `--context` on EROFS routes through the
  full rebuild; conversion warnings about dropped contexts are gone.

## Verification (structure fidelity)

- `read --info/--ls/--find/--cat` (+ all metadata flags) diffed against
  `image-inspect` on ext4 normal / sparse / sparse+shared and EROFS raw /
  sparse fixtures: identical except the documented read fixes above.
- `write --add` (append path) byte-identical to `image-repack-rework`.
  The legacy full rebuild intentionally diverges from upstream: it keeps
  contexts (shared table), skips superblock-colliding NIDs, streams file
  payloads instead of aliasing original blocks (upstream aliasing
  provably corrupted files when fresh writes landed on shared block
  numbers), and honors `--sparse-output` on stdout via temp file.
- Overlay outputs verified by content: md5 of sampled files
  (plain/inline/compressed) before/after, all operations
  (add/replace/cp/mv/mv-onto-existing/set-mode/set-owner/set-context/
  sparse/stdin); owner/mode/context inheritance checked field by field.
- Conversions verified by full recursive listing diff (names, sizes,
  types, owners, permissions — only fs-intrinsic dir sizes differ) plus
  md5 content samples, both directions and all fixtures.
- Roundtrip ext4 -> erofs -> ext4: 3000 files + all dirs byte-identical
  in type/owner/permissions/context (files, dirs, symlinks); all file
  contents md5-identical; generated EROFS passes the structural tiling
  validator (3077 inodes, exact tiling, all shared refs resolve).
- All ext4 outputs (rebuilds, grown, reserve, sparse-expanded,
  converted) pass `e2fsck -fn` (all 5 passes). No `fsck.erofs` exists
  on the host; EROFS validity is established by the tiling validator,
  full-tree reads and kernel-format review against mkfs layouts.
- `cargo test`: unit tests pass. Zero `unwrap()`/`expect()`/`panic!`
  in non-test code; all corrupt-input paths return clean errors.

## Benchmarks (debug build, `/usr/bin/time -v`)

| op | wall | peak RSS | out |
|---|---|---|---|
| overlay `mv` on 1.3 GB deflate EROFS | 2–3 s | ~7 MB | same size (1.30 GB) |
| ext4 rebuild + growth, 891 MB full image | ~3.6 s | ~7 MB | 1.07 GB raw |
| erofs -> ext4, 1.65 GB marble | ~43 s | ~11 MB | 2.82 GB raw |
| ext4 -> erofs, 891 MB shiba | ~1.5 s | ~7 MB | 942 MB raw |
| ext4 -> erofs + `--sparse-output` | ~2.3 s | ~7 MB | 890 MB sparse |

Memory discipline respected throughout: 64 KiB I/O quanta, single
pcluster decodes, per-file compact indexes freed on file switch, no
whole-image or whole-file buffering of image-sourced bytes (host/stdin
deltas are bounded by their own size). The legacy EROFS rebuild streams
every file payload instead of aliasing original blocks.
