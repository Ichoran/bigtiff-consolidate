# bigtiff-consolidate

A fast utility for consolidating BigTIFF IFDs to the end of the file for fast
network access. Files with interleaved IFDs require many seeks across the file,
which is extremely slow over network filesystems. This tool rewrites the file
with all IFDs at the end, enabling fast sequential reads.

## The Problem

When BigTIFF files are written incrementally (slice-by-slice), each IFD is
placed immediately after its image data. This means loading a 20,000-plane file
requires 20,000 seeks across the entire file - extremely slow over NFS/SMB.

Files written in one shot (or with `contiguous=True` in tifffile) place all
IFDs at the end. This allows reading all IFDs with a single seek to the end
plus a sequential read.

## Modes

### Default Mode (no flags)

Consolidates IFDs to the end while **preserving all metadata**.

```bash
./bigtiff-consolidate input.tiff
```

- If file is already consolidated: prints message and exits (exit code 0)
- If on same filesystem: renames original to `input_original.tiff`, writes
  consolidated version to `input.tiff`
- If cross-filesystem: writes consolidated version to `input_copy.tiff`

The consolidation check tolerates a small number of "out of place" IFDs
(default: 10). This handles files where the first IFD is at the beginning
but all subsequent IFDs are clustered at the end. Use `-c N` to adjust:

```bash
./bigtiff-consolidate -c 1 input.tiff   # Strict: only 1 out-of-place IFD allowed
./bigtiff-consolidate -c 20 input.tiff  # Lenient: up to 20 allowed
```

Note: `-c` only applies to default mode; `-plain` and `-ome` always rewrite.

### Plain Mode (`-plain`)

Strips all metadata tags while consolidating IFDs. Always rewrites the file
(does not check if already consolidated).

```bash
./bigtiff-consolidate -plain input.tiff
# Creates: input_plain.tiff, input_metadata.json
```

### OME-TIFF Mode (`-ome`)

Creates proper OME-TIFF with OME-XML metadata (helps ImageJ load faster).
Always rewrites the file (does not check if already consolidated).

```bash
./bigtiff-consolidate -ome input.tiff
# Creates: input.ome.tiff, input_metadata.json
```

## Building

Rust 2024 edition is required, with cargo installed to build the project.

```bash
cargo build --release
```

The binary will be at `target/release/bigtiff-consolidate`.

## Output Files

| Mode | Output | Description |
|------|--------|-------------|
| (default) | `*_original.tiff` | Backup of original (if rename possible) |
| (default) | `*_copy.tiff` | Consolidated copy (if rename not possible) |
| `-plain` | `*_plain.tiff` | BigTIFF with metadata stripped |
| `-plain` | `*_metadata.json` | Original metadata (one JSON per line) |
| `-ome` | `*.ome.tiff` | OME-TIFF with minimal OME-XML |
| `-ome` | `*_metadata.json` | Original metadata (one JSON per line) |

## Notes

- Only processes little-endian BigTIFF files
- Fails if output file already exists (won't overwrite)
- Skips writing `_metadata.json` if it already exists
- Image data is copied directly without re-encoding
- Exit code 0 if file is already consolidated (no action needed)

## CAUTION

- Check output validity before deleting valuable data.
- Not tested on compressed TIFFs.  Don't expect it to work.
- Not tested with custom tags.  If you use atypical tags for anything important, check them!

## Prevention

If you're generating files with tifffile, use `contiguous=True` to write
IFDs at the end automatically:

```python
with tifffile.TiffWriter(path, bigtiff=True) as tif:
    for plane in data:
        tif.write(plane, contiguous=True)
```
