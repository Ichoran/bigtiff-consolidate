//! BigTIFF IFD consolidator
//!
//! Consolidates IFDs to the end of file for fast network access.
//! Optionally strips metadata or creates OME-TIFF format.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process;

const BIGTIFF_MAGIC: u16 = 43;
const LITTLE_ENDIAN: u16 = 0x4949; // "II"

// Tags to keep (essential for image structure)
const KEEP_TAGS: &[u16] = &[
    256, // ImageWidth
    257, // ImageLength
    258, // BitsPerSample
    259, // Compression
    262, // PhotometricInterpretation
    273, // StripOffsets
    277, // SamplesPerPixel
    278, // RowsPerStrip
    279, // StripByteCounts
    282, // XResolution
    283, // YResolution
    284, // PlanarConfiguration
    296, // ResolutionUnit
    322, // TileWidth
    323, // TileLength
    324, // TileOffsets
    325, // TileByteCounts
    339, // SampleFormat
];

// Tags to extract as metadata (will be saved to JSON)
const METADATA_TAGS: &[(u16, &str)] = &[
    (270, "ImageDescription"),
    (271, "Make"),
    (272, "Model"),
    (305, "Software"),
    (306, "DateTime"),
    (315, "Artist"),
    (33432, "Copyright"),
];

fn tag_name(tag: u16) -> Option<&'static str> {
    METADATA_TAGS.iter().find(|(t, _)| *t == tag).map(|(_, name)| *name)
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn write_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

#[derive(Clone, Debug)]
struct IfdEntry {
    tag: u16,
    typ: u16,
    count: u64,
    value_or_offset: [u8; 8], // Raw bytes - either inline value or offset
}

impl IfdEntry {
    fn value_size(&self) -> u64 {
        let element_size: u64 = match self.typ {
            1 | 2 | 6 | 7 => 1,  // BYTE, ASCII, SBYTE, UNDEFINED
            3 | 8 => 2,          // SHORT, SSHORT
            4 | 9 | 11 => 4,     // LONG, SLONG, FLOAT
            5 | 10 | 12 => 8,    // RATIONAL, SRATIONAL, DOUBLE
            16 | 17 => 8,        // LONG8, SLONG8
            18 => 8,             // IFD8
            _ => 1,
        };
        self.count * element_size
    }

    fn is_inline(&self) -> bool {
        self.value_size() <= 8
    }

    fn offset(&self) -> u64 {
        u64::from_le_bytes(self.value_or_offset)
    }
}

struct BigTiffReader {
    reader: BufReader<File>,
}

impl BigTiffReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(1024 * 1024, file);

        // Verify BigTIFF header
        let byte_order = read_u16(&mut reader)?;
        if byte_order != LITTLE_ENDIAN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Only little-endian BigTIFF supported",
            ));
        }

        let magic = read_u16(&mut reader)?;
        if magic != BIGTIFF_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Not a BigTIFF file (magic={}, expected {})", magic, BIGTIFF_MAGIC),
            ));
        }

        let offset_size = read_u16(&mut reader)?;
        if offset_size != 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid BigTIFF offset size",
            ));
        }

        let _padding = read_u16(&mut reader)?;

        Ok(Self { reader })
    }

    fn first_ifd_offset(&mut self) -> io::Result<u64> {
        self.reader.seek(SeekFrom::Start(8))?;
        read_u64(&mut self.reader)
    }

    fn read_ifd(&mut self, offset: u64) -> io::Result<(Vec<IfdEntry>, u64)> {
        self.reader.seek(SeekFrom::Start(offset))?;
        let num_entries = read_u64(&mut self.reader)?;

        let mut entries = Vec::with_capacity(num_entries as usize);
        for _ in 0..num_entries {
            let tag = read_u16(&mut self.reader)?;
            let typ = read_u16(&mut self.reader)?;
            let count = read_u64(&mut self.reader)?;
            let mut value_or_offset = [0u8; 8];
            self.reader.read_exact(&mut value_or_offset)?;

            entries.push(IfdEntry {
                tag,
                typ,
                count,
                value_or_offset,
            });
        }

        let next_ifd = read_u64(&mut self.reader)?;
        Ok((entries, next_ifd))
    }

    fn read_value_data(&mut self, entry: &IfdEntry) -> io::Result<Vec<u8>> {
        if entry.is_inline() {
            Ok(entry.value_or_offset[..entry.value_size() as usize].to_vec())
        } else {
            let offset = entry.offset();
            self.reader.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0u8; entry.value_size() as usize];
            self.reader.read_exact(&mut data)?;
            Ok(data)
        }
    }

    fn read_strip_data(&mut self, offset: u64, size: u64, buf: &mut [u8]) -> io::Result<()> {
        self.reader.seek(SeekFrom::Start(offset))?;
        self.reader.read_exact(&mut buf[..size as usize])
    }
}

struct BigTiffWriter {
    writer: BufWriter<File>,
    position: u64,
}

impl BigTiffWriter {
    fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);

        // Write BigTIFF header
        write_u16(&mut writer, LITTLE_ENDIAN)?;
        write_u16(&mut writer, BIGTIFF_MAGIC)?;
        write_u16(&mut writer, 8)?; // offset size
        write_u16(&mut writer, 0)?; // padding

        // Placeholder for first IFD offset
        write_u64(&mut writer, 0)?;

        Ok(Self {
            writer,
            position: 16,
        })
    }

    fn write_first_ifd_offset(&mut self, offset: u64) -> io::Result<()> {
        self.writer.seek(SeekFrom::Start(8))?;
        write_u64(&mut self.writer, offset)?;
        self.writer.seek(SeekFrom::Start(self.position))?;
        Ok(())
    }

    fn current_position(&self) -> u64 {
        self.position
    }

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<u64> {
        let pos = self.position;
        self.writer.write_all(data)?;
        self.position += data.len() as u64;
        Ok(pos)
    }

    fn write_ifd(
        &mut self,
        entries: &[IfdEntry],
        next_ifd: u64,
    ) -> io::Result<u64> {
        let ifd_offset = self.position;

        write_u64(&mut self.writer, entries.len() as u64)?;
        self.position += 8;

        for entry in entries {
            write_u16(&mut self.writer, entry.tag)?;
            write_u16(&mut self.writer, entry.typ)?;
            write_u64(&mut self.writer, entry.count)?;
            self.writer.write_all(&entry.value_or_offset)?;
            self.position += 20;
        }

        write_u64(&mut self.writer, next_ifd)?;
        self.position += 8;

        Ok(ifd_offset)
    }

    fn update_next_ifd(&mut self, ifd_offset: u64, next_ifd: u64, num_entries: u64) -> io::Result<()> {
        // next_ifd is at: ifd_offset + 8 (num_entries) + num_entries * 20 (entries)
        let next_ifd_pos = ifd_offset + 8 + num_entries * 20;
        let current = self.position;
        self.writer.seek(SeekFrom::Start(next_ifd_pos))?;
        write_u64(&mut self.writer, next_ifd)?;
        self.writer.seek(SeekFrom::Start(current))?;
        Ok(())
    }

    fn align(&mut self, alignment: u64) -> io::Result<()> {
        let remainder = self.position % alignment;
        if remainder != 0 {
            let padding = alignment - remainder;
            for _ in 0..padding {
                self.writer.write_all(&[0])?;
            }
            self.position += padding;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn get_offsets_and_counts(entries: &[IfdEntry], reader: &mut BigTiffReader) -> io::Result<(Vec<u64>, Vec<u64>)> {
    let mut offsets = Vec::new();
    let mut counts = Vec::new();

    for entry in entries {
        match entry.tag {
            273 | 324 => { // StripOffsets or TileOffsets
                let data = reader.read_value_data(entry)?;
                offsets = parse_offsets(&data, entry.typ, entry.count);
            }
            279 | 325 => { // StripByteCounts or TileByteCounts
                let data = reader.read_value_data(entry)?;
                counts = parse_offsets(&data, entry.typ, entry.count);
            }
            _ => {}
        }
    }

    Ok((offsets, counts))
}

fn parse_offsets(data: &[u8], typ: u16, count: u64) -> Vec<u64> {
    let mut result = Vec::with_capacity(count as usize);
    match typ {
        3 => { // SHORT
            for i in 0..count as usize {
                let v = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                result.push(v as u64);
            }
        }
        4 => { // LONG
            for i in 0..count as usize {
                let v = u32::from_le_bytes([
                    data[i * 4],
                    data[i * 4 + 1],
                    data[i * 4 + 2],
                    data[i * 4 + 3],
                ]);
                result.push(v as u64);
            }
        }
        16 => { // LONG8
            for i in 0..count as usize {
                let v = u64::from_le_bytes([
                    data[i * 8],
                    data[i * 8 + 1],
                    data[i * 8 + 2],
                    data[i * 8 + 3],
                    data[i * 8 + 4],
                    data[i * 8 + 5],
                    data[i * 8 + 6],
                    data[i * 8 + 7],
                ]);
                result.push(v);
            }
        }
        _ => {}
    }
    result
}

fn encode_offsets(offsets: &[u64]) -> (u16, Vec<u8>) {
    // Always use LONG8 for BigTIFF
    let typ = 16u16;
    let mut data = Vec::with_capacity(offsets.len() * 8);
    for &o in offsets {
        data.extend_from_slice(&o.to_le_bytes());
    }
    (typ, data)
}

fn extract_image_dimensions(entries: &[IfdEntry]) -> (u32, u32, u16) {
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bits = 16u16;

    for entry in entries {
        match entry.tag {
            256 => { // ImageWidth
                width = u32::from_le_bytes([
                    entry.value_or_offset[0],
                    entry.value_or_offset[1],
                    entry.value_or_offset[2],
                    entry.value_or_offset[3],
                ]);
            }
            257 => { // ImageLength
                height = u32::from_le_bytes([
                    entry.value_or_offset[0],
                    entry.value_or_offset[1],
                    entry.value_or_offset[2],
                    entry.value_or_offset[3],
                ]);
            }
            258 => { // BitsPerSample
                bits = u16::from_le_bytes([
                    entry.value_or_offset[0],
                    entry.value_or_offset[1],
                ]);
            }
            _ => {}
        }
    }

    (width, height, bits)
}

fn generate_ome_xml(width: u32, height: u32, num_z: u64, bits: u16) -> String {
    let pixel_type = match bits {
        8 => "uint8",
        16 => "uint16",
        32 => "uint32",
        _ => "uint16",
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06"
     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
     xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">
  <Image ID="Image:0" Name="image">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="{}"
            SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="1" SizeT="1"
            BigEndian="false" Interleaved="false">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
      <TiffData IFD="0" PlaneCount="{}"/>
    </Pixels>
  </Image>
</OME>"#,
        pixel_type, width, height, num_z, num_z
    )
}

fn create_ome_description_entry(ome_xml: &str, writer: &mut BigTiffWriter) -> io::Result<IfdEntry> {
    let mut data = ome_xml.as_bytes().to_vec();
    data.push(0); // null terminator

    let count = data.len() as u64;
    let mut entry = IfdEntry {
        tag: 270, // ImageDescription
        typ: 2,   // ASCII
        count,
        value_or_offset: [0u8; 8],
    };

    if data.len() <= 8 {
        entry.value_or_offset[..data.len()].copy_from_slice(&data);
    } else {
        writer.align(8)?;
        let offset = writer.write_bytes(&data)?;
        entry.value_or_offset = offset.to_le_bytes();
    }

    Ok(entry)
}

fn extract_metadata(entries: &[IfdEntry], reader: &mut BigTiffReader) -> io::Result<HashMap<&'static str, String>> {
    let mut metadata = HashMap::new();

    for entry in entries {
        if let Some(name) = tag_name(entry.tag) {
            // Only extract ASCII string tags
            if entry.typ == 2 {
                let data = reader.read_value_data(entry)?;
                // Convert to string, trimming null terminator
                let s = String::from_utf8_lossy(&data);
                let s = s.trim_end_matches('\0').to_string();
                if !s.is_empty() {
                    metadata.insert(name, s);
                }
            }
        }
    }

    Ok(metadata)
}

fn escape_json_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 2);
    result.push('"');
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

fn is_valid_json(s: &str) -> bool {
    let mut bytes = s.as_bytes().to_vec();
    simd_json::from_slice::<simd_json::OwnedValue>(&mut bytes).is_ok()
}

fn metadata_to_json_line(metadata: &HashMap<&'static str, String>) -> String {
    if metadata.is_empty() {
        return "{}".to_string();
    }

    let mut parts: Vec<String> = metadata
        .iter()
        .map(|(k, v)| {
            let value = if is_valid_json(v) {
                v.clone() // Already valid JSON, embed directly
            } else {
                escape_json_string(v) // Escape as string
            };
            format!("{}:{}", escape_json_string(k), value)
        })
        .collect();
    parts.sort(); // Consistent ordering

    format!("{{{}}}", parts.join(","))
}

/// Check if IFDs are already consolidated at the end of the file.
///
/// In a consolidated file: all image data at low addresses, all IFDs at high addresses.
/// In an interleaved file: IFDs and image data are mixed throughout.
///
/// We track IFD positions in a sorted set and max(image_end) as we scan. A crossing
/// occurs when:
/// - A new IFD appears before max(image_end) seen so far
/// - New image data extends past any IFD position (those IFDs are removed as outliers)
///
/// This allows a small number of "out of place" IFDs (e.g., the first IFD at the
/// beginning of the file) without triggering a full rewrite.
///
/// If we see more than `allowed_crossings` crossings, the file is interleaved.
fn check_ifds_consolidated(reader: &mut BigTiffReader, allowed_crossings: u32) -> io::Result<bool> {
    let mut ifd_offset = reader.first_ifd_offset()?;

    // Track IFD positions in sorted order so we can remove outliers
    let mut ifd_positions: BTreeSet<u64> = BTreeSet::new();
    let mut max_image_end: u64 = 0;
    let mut crossings: u32 = 0;

    while ifd_offset != 0 {
        // Check if this IFD is before any image data we've seen (crossing)
        if ifd_offset < max_image_end {
            crossings += 1;
            if crossings > allowed_crossings {
                return Ok(false);
            }
            // Don't add this IFD to the set - it's already counted as out of place
        } else {
            // Add this IFD position to our sorted set
            ifd_positions.insert(ifd_offset);
        }

        let (entries, next_ifd) = reader.read_ifd(ifd_offset)?;
        let (offsets, counts) = get_offsets_and_counts(&entries, reader)?;

        // Check each image data block
        for (&off, &cnt) in offsets.iter().zip(counts.iter()) {
            let end = off + cnt;

            // Remove any IFDs that are below this image data end
            // (they are "out of place" - in the image data region)
            while let Some(&min_ifd) = ifd_positions.first() {
                if min_ifd < end {
                    ifd_positions.pop_first();
                    crossings += 1;
                    if crossings > allowed_crossings {
                        return Ok(false);
                    }
                } else {
                    break;
                }
            }

            // Update max image end
            if end > max_image_end {
                max_image_end = end;
            }
        }

        ifd_offset = next_ifd;
    }

    Ok(true)
}

/// Check if we can rename on this filesystem (same device).
/// Returns true if the source and destination would be on the same mount point.
fn can_rename_in_place(path: &Path) -> bool {
    // Try to get the parent directory
    let parent = path.parent().unwrap_or(Path::new("."));

    // On Unix, we can check device IDs
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(src_meta), Ok(dst_meta)) = (path.metadata(), parent.metadata()) {
            return src_meta.dev() == dst_meta.dev();
        }
    }

    // Default: assume we can rename (same directory)
    true
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Consolidate, // Default: just consolidate IFDs, preserve all data
    Plain,       // Strip metadata, output _plain.tiff
    Ome,         // Strip metadata, add OME-XML, output .ome.tiff
}

/// Process file: consolidate IFDs to end, optionally strip metadata.
fn process_file(
    input_path: &Path,
    output_path: &Path,
    metadata_path: Option<&Path>,
    mode: Mode,
) -> io::Result<()> {
    let mut reader = BigTiffReader::open(input_path)?;
    let mut writer = BigTiffWriter::create(output_path)?;

    let keep_tags: std::collections::HashSet<u16> = KEEP_TAGS.iter().copied().collect();
    let metadata_tag_set: std::collections::HashSet<u16> = METADATA_TAGS.iter().map(|(t, _)| *t).collect();

    let mut ifd_offset = reader.first_ifd_offset()?;
    let mut ifd_count = 0u64;

    // First pass: collect all IFD info and metadata
    let mut all_ifds: Vec<(Vec<IfdEntry>, Vec<u64>, Vec<u64>)> = Vec::new();
    let mut all_metadata: Vec<HashMap<&'static str, String>> = Vec::new();
    let mut image_dims: Option<(u32, u32, u16)> = None;

    eprintln!("Reading IFDs...");
    while ifd_offset != 0 {
        let (entries, next_ifd) = reader.read_ifd(ifd_offset)?;
        let (offsets, counts) = get_offsets_and_counts(&entries, &mut reader)?;

        // Get dimensions from first IFD
        if image_dims.is_none() {
            image_dims = Some(extract_image_dimensions(&entries));
        }

        // Extract metadata before filtering (for JSON export)
        if mode != Mode::Consolidate {
            let metadata = extract_metadata(&entries, &mut reader)?;
            all_metadata.push(metadata);
        }

        // Filter entries based on mode
        let filtered: Vec<IfdEntry> = if mode == Mode::Consolidate {
            // Keep all entries
            entries
        } else {
            // Strip metadata tags
            entries
                .into_iter()
                .filter(|e| keep_tags.contains(&e.tag) && !metadata_tag_set.contains(&e.tag))
                .collect()
        };

        all_ifds.push((filtered, offsets, counts));

        ifd_count += 1;
        if ifd_count % 1000 == 0 {
            eprintln!("  Read {} IFDs...", ifd_count);
        }

        ifd_offset = next_ifd;
    }

    let (width, height, bits) = image_dims.unwrap_or((0, 0, 16));
    eprintln!("Total IFDs: {} ({}x{}, {}-bit)", ifd_count, width, height, bits);

    // Write metadata JSON if path provided (only for -plain and -ome modes)
    if let Some(meta_path) = metadata_path {
        eprintln!("Writing metadata to {}...", meta_path.display());
        let mut meta_file = BufWriter::new(File::create(meta_path)?);
        for metadata in &all_metadata {
            writeln!(meta_file, "{}", metadata_to_json_line(metadata))?;
        }
        meta_file.flush()?;
    }

    // Generate OME-XML if requested
    let ome_xml = if mode == Mode::Ome {
        let xml = generate_ome_xml(width, height, ifd_count, bits);
        eprintln!("Generated OME-XML ({} bytes)", xml.len());
        Some(xml)
    } else {
        None
    };

    // Allocate a buffer for strip data (reuse across strips)
    let max_strip_size: u64 = all_ifds
        .iter()
        .flat_map(|(_, _, counts)| counts.iter())
        .copied()
        .max()
        .unwrap_or(0);
    let mut strip_buf = vec![0u8; max_strip_size as usize];

    // =======================================================================
    // PHASE 1: Write all image data first (contiguously)
    // =======================================================================
    eprintln!("Writing image data...");
    let mut all_new_offsets: Vec<Vec<u64>> = Vec::with_capacity(all_ifds.len());
    let mut written_images = 0u64;

    for (_entries, src_offsets, src_counts) in all_ifds.iter() {
        let mut new_offsets = Vec::with_capacity(src_offsets.len());

        for (&src_off, &count) in src_offsets.iter().zip(src_counts.iter()) {
            writer.align(2)?;
            let new_off = writer.current_position();
            reader.read_strip_data(src_off, count, &mut strip_buf)?;
            writer.write_bytes(&strip_buf[..count as usize])?;
            new_offsets.push(new_off);
        }

        all_new_offsets.push(new_offsets);
        written_images += 1;

        if written_images % 1000 == 0 {
            eprintln!("  Written {} images...", written_images);
        }
    }

    // =======================================================================
    // PHASE 2: Write all IFDs at the end (contiguously) for fast network access
    // =======================================================================
    eprintln!("Writing IFDs at end of file...");
    writer.align(8)?;

    let mut ifd_infos: Vec<(u64, u64)> = Vec::with_capacity(all_ifds.len()); // (offset, num_entries)

    for (ifd_idx, (entries, _src_offsets, _src_counts)) in all_ifds.iter().enumerate() {
        let new_offsets = &all_new_offsets[ifd_idx];

        // Build new entries with updated offsets
        let mut new_entries: Vec<IfdEntry> = Vec::with_capacity(entries.len() + 1);

        for entry in entries {
            let mut new_entry = entry.clone();

            if entry.tag == 273 || entry.tag == 324 {
                // StripOffsets or TileOffsets - update with new offsets
                let (typ, data) = encode_offsets(new_offsets);
                new_entry.typ = typ;
                new_entry.count = new_offsets.len() as u64;
                if data.len() <= 8 {
                    new_entry.value_or_offset = [0u8; 8];
                    new_entry.value_or_offset[..data.len()].copy_from_slice(&data);
                } else {
                    // Write offset array data right before IFD
                    writer.align(8)?;
                    let offset = writer.write_bytes(&data)?;
                    new_entry.value_or_offset = offset.to_le_bytes();
                }
            } else if entry.tag == 279 || entry.tag == 325 {
                // StripByteCounts or TileByteCounts
                if !entry.is_inline() {
                    let data = reader.read_value_data(entry)?;
                    writer.align(8)?;
                    let offset = writer.write_bytes(&data)?;
                    new_entry.value_or_offset = offset.to_le_bytes();
                }
            } else if !entry.is_inline() {
                // Other external data - copy
                let data = reader.read_value_data(entry)?;
                writer.align(8)?;
                let offset = writer.write_bytes(&data)?;
                new_entry.value_or_offset = offset.to_le_bytes();
            }

            new_entries.push(new_entry);
        }

        // Add OME-XML to first IFD only (for -ome mode)
        if ifd_idx == 0 {
            if let Some(ref xml) = ome_xml {
                let ome_entry = create_ome_description_entry(xml, &mut writer)?;
                new_entries.push(ome_entry);
            }
        }

        // Sort entries by tag (TIFF requirement)
        new_entries.sort_by_key(|e| e.tag);

        // Write IFD (next_ifd will be updated in next pass)
        writer.align(8)?;
        let this_ifd_offset = writer.write_ifd(&new_entries, 0)?;
        ifd_infos.push((this_ifd_offset, new_entries.len() as u64));

        if (ifd_idx + 1) % 1000 == 0 {
            eprintln!("  Written {} IFDs...", ifd_idx + 1);
        }
    }

    // =======================================================================
    // PHASE 3: Update IFD chain pointers
    // =======================================================================
    eprintln!("Linking IFD chain...");

    // Set first IFD offset in header
    if let Some(&(first_offset, _)) = ifd_infos.first() {
        writer.write_first_ifd_offset(first_offset)?;
    }

    // Link each IFD to the next
    for i in 0..ifd_infos.len() - 1 {
        let (this_offset, this_count) = ifd_infos[i];
        let (next_offset, _) = ifd_infos[i + 1];
        writer.update_next_ifd(this_offset, next_offset, this_count)?;
    }

    writer.flush()?;
    eprintln!("Done! Wrote {} IFDs (contiguous at end of file).", ifd_infos.len());

    Ok(())
}

const DEFAULT_ALLOWED_CROSSINGS: u32 = 10;

fn print_usage(program: &str) {
    eprintln!("Usage: {} [OPTIONS] <input.tiff>", program);
    eprintln!();
    eprintln!("Consolidates BigTIFF IFDs to end of file for fast network access.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  (none)   Consolidate IFDs to end, preserve all metadata");
    eprintln!("           If rename is possible: renames original to *_original.tiff");
    eprintln!("           If rename not possible: writes to *_copy.tiff");
    eprintln!("  -plain   Strip metadata, write to *_plain.tiff and *_metadata.json");
    eprintln!("  -ome     Create OME-TIFF, write to *.ome.tiff and *_metadata.json");
    eprintln!("  -c N     Allowed out-of-place IFDs before rewriting (default: {}, min: 1)", DEFAULT_ALLOWED_CROSSINGS);
    eprintln!();
    eprintln!("Exit codes:");
    eprintln!("  0  Success (or file already consolidated)");
    eprintln!("  1  Error");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Parse arguments
    let mut mode = Mode::Consolidate;
    let mut allowed_crossings = DEFAULT_ALLOWED_CROSSINGS;
    let mut input_arg: Option<&String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage(&args[0]);
                process::exit(0);
            }
            "-plain" => mode = Mode::Plain,
            "-ome" => mode = Mode::Ome,
            "-c" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: -c requires a number");
                    process::exit(1);
                }
                allowed_crossings = match args[i].parse::<u32>() {
                    Ok(n) if n >= 1 => n,
                    _ => {
                        eprintln!("Error: -c requires a positive integer (min 1)");
                        process::exit(1);
                    }
                };
            }
            arg if arg.starts_with('-') => {
                eprintln!("Error: Unknown option: {}", arg);
                print_usage(&args[0]);
                process::exit(1);
            }
            _ => {
                if input_arg.is_some() {
                    eprintln!("Error: Multiple input files specified");
                    process::exit(1);
                }
                input_arg = Some(&args[i]);
            }
        }
        i += 1;
    }

    let input_arg = match input_arg {
        Some(arg) => arg,
        None => {
            print_usage(&args[0]);
            process::exit(1);
        }
    };

    let input_path = Path::new(input_arg);

    if !input_path.exists() {
        eprintln!("Error: Input file does not exist: {}", input_path.display());
        process::exit(1);
    }

    // Check if already consolidated (only in default mode)
    if mode == Mode::Consolidate {
        eprintln!("Checking if IFDs are already consolidated (tolerance: {} crossings)...", allowed_crossings);
        match BigTiffReader::open(input_path).and_then(|mut r| check_ifds_consolidated(&mut r, allowed_crossings)) {
            Ok(true) => {
                eprintln!("File is already consolidated (IFDs at end). Nothing to do.");
                process::exit(0);
            }
            Ok(false) => {
                eprintln!("IFDs are interleaved with image data. Consolidating...");
            }
            Err(e) => {
                eprintln!("Error checking file: {}", e);
                process::exit(1);
            }
        }
    }

    // Generate output filenames based on mode
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let extension = input_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("tiff");
    let parent = input_path.parent().unwrap_or(Path::new("."));

    let (output_path, backup_path, metadata_path) = match mode {
        Mode::Consolidate => {
            // Check if we can rename in place
            if can_rename_in_place(input_path) {
                // Will rename original to _original, write consolidated to original name
                let backup_name = format!("{}_original.{}", stem, extension);
                let backup_path = parent.join(&backup_name);
                (input_path.to_path_buf(), Some(backup_path), None)
            } else {
                // Write to _copy, leave original in place
                let output_name = format!("{}_copy.{}", stem, extension);
                let output_path = parent.join(&output_name);
                (output_path, None, None)
            }
        }
        Mode::Plain => {
            let output_name = format!("{}_plain.{}", stem, extension);
            let metadata_name = format!("{}_metadata.json", stem);
            (
                parent.join(&output_name),
                None,
                Some(parent.join(&metadata_name)),
            )
        }
        Mode::Ome => {
            let output_name = format!("{}.ome.tiff", stem);
            let metadata_name = format!("{}_metadata.json", stem);
            (
                parent.join(&output_name),
                None,
                Some(parent.join(&metadata_name)),
            )
        }
    };

    // Handle consolidate mode with rename
    let actual_output_path = if mode == Mode::Consolidate && backup_path.is_some() {
        let backup = backup_path.as_ref().unwrap();

        if backup.exists() {
            eprintln!("Error: Backup file already exists: {}", backup.display());
            process::exit(1);
        }

        // Create temporary output file
        let temp_name = format!("{}_consolidating.{}", stem, extension);
        let temp_path = parent.join(&temp_name);

        if temp_path.exists() {
            eprintln!("Error: Temporary file already exists: {}", temp_path.display());
            process::exit(1);
        }

        temp_path
    } else {
        if output_path.exists() {
            eprintln!("Error: Output file already exists: {}", output_path.display());
            process::exit(1);
        }
        output_path.clone()
    };

    // Check if metadata file exists (skip writing if so)
    let write_metadata = if let Some(ref meta_path) = metadata_path {
        if meta_path.exists() {
            eprintln!("Metadata file already exists, skipping: {}", meta_path.display());
            None
        } else {
            Some(meta_path.as_path())
        }
    } else {
        None
    };

    eprintln!("Input:  {}", input_path.display());
    eprintln!("Output: {}", actual_output_path.display());
    match mode {
        Mode::Consolidate => {
            if let Some(ref backup) = backup_path {
                eprintln!("Backup: {}", backup.display());
            }
            eprintln!("Mode:   Consolidate (preserve all data)");
        }
        Mode::Plain => eprintln!("Mode:   Plain (strip metadata)"),
        Mode::Ome => eprintln!("Mode:   OME-TIFF"),
    }

    if let Err(e) = process_file(input_path, &actual_output_path, write_metadata, mode) {
        eprintln!("Error: {}", e);
        // Try to clean up partial output
        let _ = fs::remove_file(&actual_output_path);
        if let Some(meta_path) = write_metadata {
            let _ = fs::remove_file(meta_path);
        }
        process::exit(1);
    }

    // For consolidate mode with rename: do the rename dance
    if mode == Mode::Consolidate {
        if let Some(ref backup) = backup_path {
            eprintln!("Renaming original to backup...");
            if let Err(e) = fs::rename(input_path, backup) {
                eprintln!("Error renaming original to backup: {}", e);
                eprintln!("Consolidated file is at: {}", actual_output_path.display());
                process::exit(1);
            }

            eprintln!("Renaming consolidated to original name...");
            if let Err(e) = fs::rename(&actual_output_path, input_path) {
                eprintln!("Error renaming consolidated file: {}", e);
                eprintln!("Backup is at: {}", backup.display());
                eprintln!("Consolidated file is at: {}", actual_output_path.display());
                process::exit(1);
            }

            eprintln!("Success! Original backed up to: {}", backup.display());
        }
    }
}
