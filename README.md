# bigtiff-strip

A fast utility for converting BigTIFF files to cleaner formats that may load
faster in ImageJ or with other slower readers.

## Formats

This tool creates either of two cleaned-up versions of BigTIFF files:

1.  **Plain BigTIFF** (`_plain.tiff`): Strips all metadata tags
(ImageDescription, Software, etc.) while preserving image data.  Fast to
create, may still be slow in ImageJ.

2.  **OME-TIFF** (`.ome.tiff`): Adds proper OME-XML metadata to the first
IFD only, telling ImageJ the exact dimensions upfront.  This allows ImageJ
to skip its slow metadata inference.

Both modes also extract the original metadata to a JSON file (`_metadata.json`) with one JSON object per plane.

## Building

Rust 2024+ is required, with cargo installed to build the project.

```bash
cargo build --release
```

The binary will be at `target/release/bigtiff-strip`.

## Usage

```bash
# Create plain BigTIFF (strips all metadata)
./target/release/bigtiff-strip input.tiff
# Creates: input_plain.tiff, input_metadata.json

# Create OME-TIFF (recommended for ImageJ)
./target/release/bigtiff-strip -ome input.tiff
# Creates: input.ome.tiff, input_metadata.json
```

## Output Files

| File | Description |
|------|-------------|
| `*_plain.tiff` | BigTIFF with metadata stripped |
| `*.ome.tiff` | OME-TIFF with minimal OME-XML (use `-ome`) |
| `*_metadata.json` | Original metadata, one JSON line per plane |

## Notes

- Only processes little-endian BigTIFF files
- Fails if output file already exists (won't overwrite)
- Skips writing `_metadata.json` if it already exists
- Image data is copied directly without re-encoding
