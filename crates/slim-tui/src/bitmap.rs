//! Clipboard bitmap → PNG (DESIGN-SLIM-TUI §20).
//!
//! Windows hands clipboard rasters over as a DIB (`CF_DIB`/`CF_DIBV5`). The
//! attachment pipeline loads image *files* (`/image PATH` → `load_local_images`),
//! so a pasted bitmap is materialized as a real PNG. The encoder below is
//! dependency-free and deterministic: filter-0 rows inside a DEFLATE stream of
//! fixed Huffman codes and greedy LZ77 matches (RFC1951 §3.2.6).

/// Pixel budget for one pasted bitmap: bounds the transient RGBA buffer.
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Encodes a DIB payload as a PNG image. Errors are user-facing reasons.
pub fn png_from_dib(dib: &[u8]) -> Result<Vec<u8>, String> {
    Ok(encode_png(&raster_from_dib(dib)?))
}

/// Cuts a clipboard `PNG` payload at the end of its `IEND` chunk: the Windows
/// clipboard reports the allocation granule, not the payload written by the
/// source application. Returns `None` when the bytes are not a complete PNG.
pub fn trim_png(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 8 || bytes[..8] != PNG_SIGNATURE {
        return None;
    }
    let mut offset = 8usize;
    while offset + 12 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        let end = offset.checked_add(12)?.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        offset = end;
        if kind == b"IEND" {
            return Some(&bytes[..offset]);
        }
    }
    None
}

struct Raster {
    width: u32,
    height: u32,
    /// Top-down RGBA rows.
    data: Vec<u8>,
}

fn raster_from_dib(dib: &[u8]) -> Result<Raster, String> {
    if dib.len() < 40 {
        return Err("truncated bitmap header".into());
    }
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;

    let header_size = le_u32(dib, 0)?;
    let width = le_i32(dib, 4)?;
    let height = le_i32(dib, 8)?;
    let planes = le_u16(dib, 12)?;
    let bpp = le_u16(dib, 14)?;
    let compression = le_u32(dib, 16)?;
    let colors_used = le_u32(dib, 32)?;

    if planes != 1 {
        return Err(format!("unsupported bitmap planes: {planes}"));
    }
    if width <= 0 || height == 0 {
        return Err("invalid bitmap size".into());
    }
    if !matches!(bpp, 24 | 32) {
        return Err(format!("unsupported {bpp}-bit bitmap"));
    }
    let width = width as u32;
    let (height, top_down) = if height < 0 {
        (height.unsigned_abs(), true)
    } else {
        (height as u32, false)
    };
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(format!(
            "bitmap is {} megapixels, over the {} megapixel limit",
            pixels / 1_000_000,
            MAX_PIXELS / 1_000_000
        ));
    }

    let opaque = match compression {
        BI_RGB => true,
        BI_BITFIELDS => {
            if bpp != 32 {
                return Err(format!("unsupported {bpp}-bit bitfields bitmap"));
            }
            // BITMAPINFOHEADER keeps the three masks right after the header;
            // V4/V5 headers carry them in the header itself. Offsets 40..52 are
            // the same in both layouts.
            let masks = (le_u32(dib, 40)?, le_u32(dib, 44)?, le_u32(dib, 48)?);
            if masks != (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) {
                return Err("unsupported bitmap color masks".into());
            }
            let alpha_mask = if header_size >= 56 {
                le_u32(dib, 52)?
            } else {
                0
            };
            alpha_mask != 0xFF00_0000
        }
        other => return Err(format!("unsupported bitmap compression: {other}")),
    };

    let trailing_masks = if compression == BI_BITFIELDS && header_size == 40 {
        12
    } else {
        0
    };
    let offset = header_size as usize + trailing_masks + colors_used as usize * 4;
    let unpadded = width as usize * (bpp as usize / 8);
    let stride = (unpadded + 3) & !3;
    let required = offset + stride * height as usize;
    if dib.len() < required {
        return Err("truncated bitmap pixels".into());
    }

    let mut data = Vec::with_capacity(pixels as usize * 4);
    for row in 0..height as usize {
        let source = if top_down {
            row
        } else {
            height as usize - 1 - row
        };
        let start = offset + source * stride;
        let row_bytes = &dib[start..start + unpadded];
        match bpp {
            24 => {
                for pixel in row_bytes.as_chunks::<3>().0 {
                    data.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xFF]);
                }
            }
            _ => {
                for pixel in row_bytes.as_chunks::<4>().0 {
                    data.extend_from_slice(&[
                        pixel[2],
                        pixel[1],
                        pixel[0],
                        if opaque { 0xFF } else { pixel[3] },
                    ]);
                }
            }
        }
    }
    Ok(Raster {
        width,
        height,
        data,
    })
}

fn encode_png(raster: &Raster) -> Vec<u8> {
    let opaque = raster
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[3] == 0xFF);
    let channels = if opaque { 3 } else { 4 };
    let mut filtered =
        Vec::with_capacity((raster.width as usize * channels + 1) * raster.height as usize);
    for row in raster.data.chunks_exact(raster.width as usize * 4) {
        filtered.push(0);
        if opaque {
            for pixel in row.as_chunks::<4>().0 {
                filtered.extend_from_slice(&pixel[..3]);
            }
        } else {
            filtered.extend_from_slice(row);
        }
    }

    let mut png = Vec::with_capacity(filtered.len() / 3 + 64);
    png.extend_from_slice(&PNG_SIGNATURE);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&raster.width.to_be_bytes());
    header.extend_from_slice(&raster.height.to_be_bytes());
    header.extend_from_slice(&[8, if opaque { 2 } else { 6 }, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &header);
    push_chunk(&mut png, b"IDAT", &zlib(&filtered));
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(crc32(0xFFFF_FFFF, kind), data);
    out.extend_from_slice(&(!crc).to_be_bytes());
}

fn crc32(state: u32, data: &[u8]) -> u32 {
    let mut crc = state;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 3 + 32);
    out.extend_from_slice(&[0x78, 0x01]);
    out.extend_from_slice(&deflate_fixed(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5_552) {
        for byte in chunk {
            a += u32::from(*byte);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

const WINDOW: usize = 32_768;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const MAX_CHAIN: usize = 48;
const HASH_BITS: u32 = 14;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// Fixed-Huffman DEFLATE (RFC1951 §3.2.6) with greedy LZ77 matching.
fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut writer = BitWriter::default();
    writer.write_bits(1, 1);
    writer.write_bits(1, 2);
    let mut head = vec![-1i32; HASH_SIZE];
    let mut previous = vec![-1i32; WINDOW];
    let mut position = 0usize;
    while position < data.len() {
        let (length, distance) = longest_match(data, position, &head, &previous);
        if length >= MIN_MATCH {
            write_match(&mut writer, length, distance);
            for insert in position..position + length {
                insert_position(data, insert, &mut head, &mut previous);
            }
            position += length;
        } else {
            write_symbol(&mut writer, u16::from(data[position]));
            insert_position(data, position, &mut head, &mut previous);
            position += 1;
        }
    }
    write_symbol(&mut writer, 256);
    writer.finish()
}

fn hash3(data: &[u8], position: usize) -> usize {
    let chunk = &data[position..position + MIN_MATCH];
    let value = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
    (value.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

fn insert_position(data: &[u8], position: usize, head: &mut [i32], previous: &mut [i32]) {
    if position + MIN_MATCH > data.len() {
        return;
    }
    let slot = hash3(data, position);
    previous[position & (WINDOW - 1)] = head[slot];
    head[slot] = position as i32;
}

fn longest_match(data: &[u8], position: usize, head: &[i32], previous: &[i32]) -> (usize, usize) {
    if position + MIN_MATCH > data.len() {
        return (0, 0);
    }
    let limit = MAX_MATCH.min(data.len() - position);
    let mut best = (0usize, 0usize);
    let mut candidate = head[hash3(data, position)];
    let mut chain = 0usize;
    while candidate >= 0 && chain < MAX_CHAIN {
        let start = candidate as usize;
        let distance = position - start;
        if distance == 0 || distance > WINDOW {
            break;
        }
        if best.0 < limit {
            let mut length = 0usize;
            while length < limit && data[start + length] == data[position + length] {
                length += 1;
            }
            if length > best.0 {
                best = (length, distance);
            }
        }
        candidate = previous[start & (WINDOW - 1)];
        chain += 1;
    }
    if best.0 < MIN_MATCH {
        (0, 0)
    } else {
        best
    }
}

fn write_symbol(writer: &mut BitWriter, symbol: u16) {
    let (code, bits) = match symbol {
        0..=143 => (0x30 + u32::from(symbol), 8),
        144..=255 => (0x190 + u32::from(symbol) - 144, 9),
        256..=279 => (u32::from(symbol) - 256, 7),
        _ => (0xC0 + u32::from(symbol) - 280, 8),
    };
    writer.write_code(code, bits);
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn write_match(writer: &mut BitWriter, length: usize, distance: usize) {
    let length_index = code_index(&LENGTH_BASE, &LENGTH_EXTRA, length)
        .expect("match length within a fixed Huffman length code");
    write_symbol(writer, 257 + length_index as u16);
    write_extra(
        writer,
        length - usize::from(LENGTH_BASE[length_index]),
        LENGTH_EXTRA[length_index],
    );
    let distance_index = code_index(&DISTANCE_BASE, &DISTANCE_EXTRA, distance)
        .expect("match distance within a fixed Huffman distance code");
    writer.write_code(distance_index as u32, 5);
    write_extra(
        writer,
        distance - usize::from(DISTANCE_BASE[distance_index]),
        DISTANCE_EXTRA[distance_index],
    );
}

fn write_extra(writer: &mut BitWriter, value: usize, bits: u8) {
    if bits > 0 {
        writer.write_bits(value as u32, u32::from(bits));
    }
}

fn code_index(bases: &[u16], extras: &[u8], value: usize) -> Option<usize> {
    bases.iter().enumerate().position(|(index, base)| {
        let base = usize::from(*base);
        value >= base && value < base + (1usize << extras[index])
    })
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    buffer: u32,
    bits: u32,
}

impl BitWriter {
    /// Writes `count` bits least-significant first (integers and extra bits).
    fn write_bits(&mut self, value: u32, count: u32) {
        if count == 0 {
            return;
        }
        self.buffer |= (value & ((1u32 << count) - 1)) << self.bits;
        self.bits += count;
        while self.bits >= 8 {
            self.out.push((self.buffer & 0xFF) as u8);
            self.buffer >>= 8;
            self.bits -= 8;
        }
    }

    /// Huffman codes travel most-significant bit first (RFC1951 §3.1.1).
    fn write_code(&mut self, code: u32, count: u32) {
        self.write_bits(code.reverse_bits() >> (32 - count), count);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push((self.buffer & 0xFF) as u8);
        }
        self.out
    }
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "truncated bitmap header".to_owned())?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated bitmap header".to_owned())?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_i32(bytes: &[u8], offset: usize) -> Result<i32, String> {
    Ok(le_u32(bytes, offset)? as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dib(width: i32, height: i32, bpp: u16, pixels: &[u8]) -> Vec<u8> {
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&bpp.to_le_bytes());
        dib.extend_from_slice(pixels);
        dib
    }

    /// BITMAPV5HEADER with explicit BGRA masks, as Chrome/Snipping Tool write it.
    fn dib_v5_bitfields(width: i32, height: i32, pixels: &[u8]) -> Vec<u8> {
        let mut dib = vec![0u8; 124];
        dib[0..4].copy_from_slice(&124u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes());
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        dib[52..56].copy_from_slice(&0xFF00_0000u32.to_le_bytes());
        dib[56..60].copy_from_slice(&0x7352_4742u32.to_le_bytes());
        dib.extend_from_slice(pixels);
        dib
    }

    fn row(pixels: &[[u8; 3]]) -> Vec<u8> {
        pixels.iter().flatten().copied().collect()
    }

    /// Minimal PNG reader for the encoder's own output: fixed Huffman DEFLATE
    /// only. Its tables are transcribed from RFC1951, not from the encoder.
    fn decode_png(png: &[u8]) -> (u32, u32, u8, Vec<u8>) {
        assert_eq!(&png[..8], &PNG_SIGNATURE);
        let mut offset = 8usize;
        let mut header = None;
        let mut raw = Vec::new();
        let mut saw_end = false;
        while offset + 12 <= png.len() {
            let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let data = &png[offset + 8..offset + 8 + length];
            let expected = !crc32(crc32(0xFFFF_FFFF, kind), data);
            let stored = u32::from_be_bytes(
                png[offset + 8 + length..offset + 12 + length]
                    .try_into()
                    .unwrap(),
            );
            assert_eq!(stored, expected, "chunk CRC for {:?}", kind);
            match kind {
                b"IHDR" => header = Some(data.to_vec()),
                b"IDAT" => raw = inflate(data),
                b"IEND" => saw_end = true,
                _ => panic!("unexpected chunk {:?}", kind),
            }
            offset += 12 + length;
        }
        assert!(saw_end, "IEND chunk");
        assert_eq!(offset, png.len(), "no trailing bytes");
        let header = header.expect("IHDR chunk");
        let width = u32::from_be_bytes(header[0..4].try_into().unwrap());
        let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
        assert_eq!(header[8], 8, "bit depth");
        assert_eq!(header[10..13], [0, 0, 0], "compression/filter/interlace");
        let channels = match header[9] {
            2 => 3usize,
            6 => 4usize,
            other => panic!("unexpected color type {other}"),
        };
        let mut pixels = Vec::with_capacity(width as usize * height as usize * channels);
        for row in raw.chunks_exact(width as usize * channels + 1) {
            assert_eq!(row[0], 0, "filter type");
            pixels.extend_from_slice(&row[1..]);
        }
        (width, height, header[9], pixels)
    }

    fn inflate(zlib: &[u8]) -> Vec<u8> {
        assert_eq!(zlib[..2], [0x78, 0x01]);
        let mut reader = BitReader::new(&zlib[2..]);
        let mut out = Vec::new();
        loop {
            let final_block = reader.bits(1) == 1;
            assert_eq!(reader.bits(2), 1, "fixed Huffman block");
            loop {
                let symbol = reader.symbol();
                if symbol == 256 {
                    break;
                }
                if symbol < 256 {
                    out.push(symbol as u8);
                    continue;
                }
                let index = symbol as usize - 257;
                let length = usize::from(LENGTH_BASE[index])
                    + reader.bits(u32::from(LENGTH_EXTRA[index])) as usize;
                // Distance codes are Huffman codes too: most-significant bit first.
                let distance_index = reader.code_bits(5) as usize;
                let distance = usize::from(DISTANCE_BASE[distance_index])
                    + reader.bits(u32::from(DISTANCE_EXTRA[distance_index])) as usize;
                for _ in 0..length {
                    let byte = out[out.len() - distance];
                    out.push(byte);
                }
            }
            if final_block {
                break;
            }
        }
        // The deflate stream ends on a byte boundary; the checksum follows it
        // as four big-endian bytes.
        let trailer = 2 + reader.align() / 8;
        assert_eq!(
            u32::from_be_bytes(zlib[trailer..trailer + 4].try_into().unwrap()),
            adler32(&out),
            "zlib checksum"
        );
        out
    }

    struct BitReader<'a> {
        data: &'a [u8],
        bit: usize,
    }

    impl<'a> BitReader<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self { data, bit: 0 }
        }

        fn one(&mut self) -> u32 {
            let byte = self.data[self.bit / 8];
            let value = (u32::from(byte) >> (self.bit % 8)) & 1;
            self.bit += 1;
            value
        }

        /// Least-significant first, as written by the encoder.
        fn bits(&mut self, count: u32) -> u32 {
            let mut value = 0u32;
            for index in 0..count {
                value |= self.one() << index;
            }
            value
        }

        /// Most-significant first, as written for Huffman codes and raw bytes.
        fn code_bits(&mut self, count: u32) -> u32 {
            let mut value = 0u32;
            for _ in 0..count {
                value = (value << 1) | self.one();
            }
            value
        }

        fn align(&mut self) -> usize {
            self.bit = self.bit.div_ceil(8) * 8;
            self.bit
        }

        fn symbol(&mut self) -> u16 {
            let mut code = 0u32;
            for length in 1..=9u32 {
                code = (code << 1) | self.one();
                let symbol = match length {
                    7 if code <= 0b0010111 => Some(256 + code),
                    8 if (0x30..=0xBF).contains(&code) => Some(code - 0x30),
                    8 if (0xC0..=0xC7).contains(&code) => Some(280 + code - 0xC0),
                    9 if (0x190..=0x1FF).contains(&code) => Some(144 + code - 0x190),
                    _ => None,
                };
                if let Some(symbol) = symbol {
                    return symbol as u16;
                }
            }
            panic!("invalid fixed Huffman code");
        }
    }

    #[test]
    fn bottom_up_24_bit_bitmap_keeps_pixel_order_and_row_padding() {
        let mut pixels = row(&[[0, 0, 255], [0, 255, 0]]);
        pixels.extend_from_slice(&[0, 0]);
        pixels.extend_from_slice(&row(&[[255, 0, 0], [255, 255, 255]]));
        pixels.extend_from_slice(&[0, 0]);
        let png = png_from_dib(&dib(2, 2, 24, &pixels)).expect("encode");

        let (width, height, color_type, decoded) = decode_png(&png);
        assert_eq!((width, height), (2, 2));
        assert_eq!(color_type, 2, "opaque bitmaps stay RGB");
        assert_eq!(
            decoded,
            vec![0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 0],
            "top-down rows from a bottom-up DIB"
        );
    }

    #[test]
    fn top_down_bitfields_bitmap_preserves_alpha() {
        let pixels = vec![10, 20, 30, 128, 40, 50, 60, 255];
        let png = png_from_dib(&dib_v5_bitfields(2, -1, &pixels)).expect("encode");

        let (width, height, color_type, decoded) = decode_png(&png);
        assert_eq!((width, height), (2, 1));
        assert_eq!(color_type, 6, "alpha channel survives");
        assert_eq!(
            decoded,
            vec![30, 20, 10, 128, 60, 50, 40, 255],
            "BGRA → RGBA, top-down already"
        );
    }

    #[test]
    fn rgb_bitmap_ignores_the_reserved_byte() {
        let pixels = vec![1, 2, 3, 0, 4, 5, 6, 0];
        let png = png_from_dib(&dib(2, 1, 32, &pixels)).expect("encode");

        let (_, _, color_type, decoded) = decode_png(&png);
        assert_eq!(color_type, 2);
        assert_eq!(decoded, vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn unsupported_and_truncated_bitmaps_are_visible_errors() {
        assert!(png_from_dib(&dib(2, 2, 8, &[0; 16]))
            .expect_err("8-bit")
            .contains("8-bit"));
        assert!(png_from_dib(&dib(2, 2, 24, &[0; 8]))
            .expect_err("short pixel data")
            .contains("truncated"));
        let mut compressed = dib(2, 2, 24, &[0; 64]);
        compressed[16..20].copy_from_slice(&4u32.to_le_bytes());
        assert!(png_from_dib(&compressed)
            .expect_err("compression")
            .contains("compression"));
        assert!(png_from_dib(&[0; 4]).is_err());
    }

    #[test]
    fn repeated_rows_compress_below_the_raw_filtered_size() {
        let pixels = vec![7u8; 256 * 256 * 4];
        let png = png_from_dib(&dib(256, 256, 32, &pixels)).expect("encode");
        assert!(
            png.len() < 20_000,
            "a flat 256x256 bitmap must use LZ77 matches: {} bytes",
            png.len()
        );
    }

    #[test]
    fn trim_png_cuts_the_allocation_tail_and_rejects_partial_payloads() {
        let png = png_from_dib(&dib(1, 1, 24, &[1, 2, 3, 0])).expect("encode");
        let mut padded = png.clone();
        padded.extend_from_slice(&[0; 16]);
        assert_eq!(trim_png(&padded), Some(png.as_slice()));
        assert_eq!(trim_png(&png[..png.len() - 1]), None);
        assert_eq!(trim_png(b"not a png"), None);
    }
}
