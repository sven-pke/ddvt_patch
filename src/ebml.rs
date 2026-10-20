//! Minimal EBML reader/writer.
//!
//! Only what a patcher needs: walk a buffer element by element, and re-encode
//! numbers at a width we choose. That last part is what makes in-place patching
//! possible - EBML lets a size or an integer be stored in more bytes than the
//! value strictly needs, so a field can change value without changing length.

use anyhow::{bail, Result};

// Element IDs, stored with their length marker, the way they appear on disk.
pub const ID_EBML: u32 = 0x1A45_DFA3;
pub const ID_SEGMENT: u32 = 0x1853_8067;
pub const ID_SEEK_HEAD: u32 = 0x114D_9B74;
pub const ID_SEEK: u32 = 0x4DBB;
pub const ID_SEEK_ID: u32 = 0x53AB;
pub const ID_SEEK_POSITION: u32 = 0x53AC;
pub const ID_INFO: u32 = 0x1549_A966;
pub const ID_TRACKS: u32 = 0x1654_AE6B;
pub const ID_TRACK_ENTRY: u32 = 0xAE;
pub const ID_TRACK_NUMBER: u32 = 0xD7;
pub const ID_TRACK_TYPE: u32 = 0x83;
pub const ID_CODEC_ID: u32 = 0x86;
pub const ID_CODEC_PRIVATE: u32 = 0x63A2;
pub const ID_VIDEO: u32 = 0xE0;
pub const ID_PIXEL_WIDTH: u32 = 0xB0;
pub const ID_PIXEL_HEIGHT: u32 = 0xBA;
pub const ID_COLOUR: u32 = 0x55B0;
pub const ID_MATRIX_COEFFICIENTS: u32 = 0x55B1;
pub const ID_RANGE: u32 = 0x55B9;
pub const ID_TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
pub const ID_PRIMARIES: u32 = 0x55BB;
pub const ID_MAX_CLL: u32 = 0x55BC;
pub const ID_MAX_FALL: u32 = 0x55BD;
pub const ID_MASTERING_METADATA: u32 = 0x55D0;
pub const ID_PRIMARY_R_X: u32 = 0x55D1;
pub const ID_PRIMARY_R_Y: u32 = 0x55D2;
pub const ID_PRIMARY_G_X: u32 = 0x55D3;
pub const ID_PRIMARY_G_Y: u32 = 0x55D4;
pub const ID_PRIMARY_B_X: u32 = 0x55D5;
pub const ID_PRIMARY_B_Y: u32 = 0x55D6;
pub const ID_WHITE_X: u32 = 0x55D7;
pub const ID_WHITE_Y: u32 = 0x55D8;
pub const ID_LUMINANCE_MAX: u32 = 0x55D9;
pub const ID_LUMINANCE_MIN: u32 = 0x55DA;
pub const ID_VOID: u32 = 0xEC;
pub const ID_CRC32: u32 = 0xBF;
pub const ID_CLUSTER: u32 = 0x1F43_B675;
pub const ID_CUES: u32 = 0x1C53_BB6B;
pub const ID_CHAPTERS: u32 = 0x1043_A770;
pub const ID_ATTACHMENTS: u32 = 0x1941_A469;
pub const ID_TAGS: u32 = 0x1254_C367;

/// One element header plus where its body sits in the buffer.
#[derive(Debug, Clone, Copy)]
pub struct Elem {
    pub id: u32,
    pub id_len: usize,
    /// Body length in bytes. Meaningless when `unknown_size` is set.
    pub size: u64,
    pub size_len: usize,
    /// Offset of the element's first byte, relative to the buffer start.
    pub offset: usize,
    /// Offset of the element's body, relative to the buffer start.
    pub body: usize,
    pub unknown_size: bool,
}

impl Elem {
    pub fn header_len(&self) -> usize {
        self.id_len + self.size_len
    }
    pub fn total_len(&self) -> usize {
        self.header_len() + self.size as usize
    }
    pub fn body_end(&self) -> usize {
        self.body + self.size as usize
    }
}

/// Read an element ID: a vint whose length marker is kept as part of the value.
pub fn read_id(buf: &[u8], pos: usize) -> Result<(u32, usize)> {
    if pos >= buf.len() {
        bail!("EBML: element ID reads past the end of the buffer");
    }
    let b0 = buf[pos];
    if b0 == 0 {
        bail!("EBML: invalid element ID at offset {pos} (first byte is zero)");
    }
    let len = b0.leading_zeros() as usize + 1;
    if len > 4 {
        bail!("EBML: element ID at offset {pos} claims {len} bytes, max is 4");
    }
    if pos + len > buf.len() {
        bail!("EBML: element ID at offset {pos} reads past the end of the buffer");
    }
    let mut val: u32 = 0;
    for i in 0..len {
        val = (val << 8) | buf[pos + i] as u32;
    }
    Ok((val, len))
}

/// Read a size vint. Returns the value, its encoded width, and whether it is
/// the reserved "unknown size" pattern.
pub fn read_size(buf: &[u8], pos: usize) -> Result<(u64, usize, bool)> {
    if pos >= buf.len() {
        bail!("EBML: size reads past the end of the buffer");
    }
    let b0 = buf[pos];
    if b0 == 0 {
        bail!("EBML: invalid size at offset {pos} (first byte is zero)");
    }
    let len = b0.leading_zeros() as usize + 1;
    if len > 8 {
        bail!("EBML: size at offset {pos} claims {len} bytes, max is 8");
    }
    if pos + len > buf.len() {
        bail!("EBML: size at offset {pos} reads past the end of the buffer");
    }
    let mut val: u64 = (b0 & vint_mask(len)) as u64;
    for i in 1..len {
        val = (val << 8) | buf[pos + i] as u64;
    }
    let all_ones = val == (1u64 << (7 * len)) - 1;
    Ok((val, len, all_ones))
}

/// The value bits of a vint's first byte. For an eight byte vint the first
/// byte is the marker alone; shifting 0xFF by eight would overflow a u8.
fn vint_mask(len: usize) -> u8 {
    if len >= 8 {
        0
    } else {
        0xFFu8 >> len
    }
}

/// Read a plain vint (marker stripped), as used for block track numbers.
/// Returns the value and its width; `None` for a zero first byte, which no
/// valid vint starts with.
pub fn read_vint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let b0 = *buf.get(pos)?;
    if b0 == 0 {
        return None;
    }
    let len = b0.leading_zeros() as usize + 1;
    if pos + len > buf.len() {
        return None;
    }
    let mut val = (b0 & vint_mask(len)) as u64;
    for i in 1..len {
        val = (val << 8) | buf[pos + i] as u64;
    }
    Some((val, len))
}

/// Read one element header at `pos`.
pub fn read_elem(buf: &[u8], pos: usize) -> Result<Elem> {
    let (id, id_len) = read_id(buf, pos)?;
    let (size, size_len, unknown_size) = read_size(buf, pos + id_len)?;
    Ok(Elem {
        id,
        id_len,
        size,
        size_len,
        offset: pos,
        body: pos + id_len + size_len,
        unknown_size,
    })
}

/// Walk the direct children of a master element body spanning `start..end`.
///
/// Stops cleanly at the first child that would run past `end`, so it is safe to
/// call on a buffer holding only the first slice of a very large file.
pub fn children(buf: &[u8], start: usize, end: usize) -> Result<Vec<Elem>> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        let el = match read_elem(buf, pos) {
            Ok(e) => e,
            Err(_) => break,
        };
        if el.unknown_size || el.body_end() > end {
            out.push(el);
            break;
        }
        pos = el.body_end();
        out.push(el);
    }
    Ok(out)
}

/// Find the first direct child with the given ID.
pub fn find<'a>(list: &'a [Elem], id: u32) -> Option<&'a Elem> {
    list.iter().find(|e| e.id == id)
}

/// Encode an element ID back to its on-disk bytes.
pub fn write_id(id: u32, out: &mut Vec<u8>) {
    let len = if id > 0x00FF_FFFF {
        4
    } else if id > 0x0000_FFFF {
        3
    } else if id > 0x0000_00FF {
        2
    } else {
        1
    };
    for i in (0..len).rev() {
        out.push((id >> (8 * i)) as u8);
    }
}

/// Narrowest width that can hold `value` as a size vint.
pub fn min_size_width(value: u64) -> usize {
    for len in 1..=8usize {
        if value < (1u64 << (7 * len)) - 1 {
            return len;
        }
    }
    8
}

/// Encode a size vint at an exact width. EBML permits a size to be stored wider
/// than necessary, which is how a growing element can keep its header length.
pub fn write_size_at(value: u64, width: usize, out: &mut Vec<u8>) -> Result<()> {
    if width == 0 || width > 8 {
        bail!("EBML: size width {width} out of range");
    }
    if value >= (1u64 << (7 * width)) - 1 {
        bail!("EBML: value {value} does not fit in a {width}-byte size field");
    }
    let marker = 1u64 << (7 * width);
    let v = value | marker;
    for i in (0..width).rev() {
        out.push((v >> (8 * i)) as u8);
    }
    Ok(())
}

/// Narrowest big-endian byte count for an unsigned integer. Zero takes one byte,
/// matching what muxers write.
pub fn min_uint_width(value: u64) -> usize {
    let mut n = 1;
    let mut v = value >> 8;
    while v != 0 {
        n += 1;
        v >>= 8;
    }
    n
}

/// Encode an unsigned integer body at an exact width, zero-padded on the left.
pub fn write_uint_at(value: u64, width: usize, out: &mut Vec<u8>) -> Result<()> {
    if width == 0 || width > 8 {
        bail!("EBML: uint width {width} out of range");
    }
    if width < 8 && value >= (1u64 << (8 * width)) {
        bail!("EBML: value {value} does not fit in {width} bytes");
    }
    for i in (0..width).rev() {
        out.push((value >> (8 * i)) as u8);
    }
    Ok(())
}

/// Read an unsigned integer body. An absent (zero-length) body means zero.
pub fn read_uint(buf: &[u8], el: &Elem) -> Result<u64> {
    if el.body_end() > buf.len() {
        bail!("EBML: integer body runs past the end of the buffer");
    }
    if el.size > 8 {
        bail!("EBML: integer body of {} bytes is too wide", el.size);
    }
    let mut v: u64 = 0;
    for i in 0..el.size as usize {
        v = (v << 8) | buf[el.body + i] as u64;
    }
    Ok(v)
}

/// Read a float body. Matroska allows 4 or 8 bytes; a zero-length body is 0.0.
pub fn read_float(buf: &[u8], el: &Elem) -> Result<f64> {
    if el.body_end() > buf.len() {
        bail!("EBML: float body runs past the end of the buffer");
    }
    match el.size {
        0 => Ok(0.0),
        4 => {
            let b: [u8; 4] = buf[el.body..el.body + 4].try_into().unwrap();
            Ok(f32::from_be_bytes(b) as f64)
        }
        8 => {
            let b: [u8; 8] = buf[el.body..el.body + 8].try_into().unwrap();
            Ok(f64::from_be_bytes(b))
        }
        n => bail!("EBML: float body of {n} bytes is neither 4 nor 8"),
    }
}

/// Encode a float body at an exact width, so a value can change without the
/// element changing length.
pub fn write_float_at(value: f64, width: usize, out: &mut Vec<u8>) -> Result<()> {
    match width {
        4 => out.extend_from_slice(&(value as f32).to_be_bytes()),
        8 => out.extend_from_slice(&value.to_be_bytes()),
        n => bail!("EBML: float width {n} is neither 4 nor 8"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_roundtrip_keeps_the_width_we_ask_for() {
        // 1000 needs two bytes at minimum, but must also be encodable in four
        // so a field can grow in value without growing on disk.
        assert_eq!(min_size_width(1000), 2);
        let mut wide = Vec::new();
        write_size_at(1000, 4, &mut wide).unwrap();
        assert_eq!(wide.len(), 4);
        let (v, len, unknown) = read_size(&wide, 0).unwrap();
        assert_eq!((v, len, unknown), (1000, 4, false));
    }

    #[test]
    fn an_eight_byte_size_drops_its_marker() {
        // The Segment of a large file uses the full width; the marker bit sits
        // alone in the first byte and must not leak into the value.
        let mut buf = Vec::new();
        write_size_at(6_750_706_598, 8, &mut buf).unwrap();
        assert_eq!(buf[0], 0x01);
        let (v, len, unknown) = read_size(&buf, 0).unwrap();
        assert_eq!((v, len, unknown), (6_750_706_598, 8, false));
        assert_eq!(read_vint(&[0x00], 0), None);
        assert_eq!(read_vint(&[0x81], 0), Some((1, 1)));
    }

    #[test]
    fn all_ones_is_reported_as_unknown_size() {
        let buf = [0xFFu8];
        let (_, len, unknown) = read_size(&buf, 0).unwrap();
        assert_eq!(len, 1);
        assert!(unknown);
    }

    #[test]
    fn a_size_that_does_not_fit_is_refused_rather_than_truncated() {
        let mut out = Vec::new();
        assert!(write_size_at(1000, 1, &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn ids_survive_a_round_trip_with_their_marker() {
        for id in [ID_VOID, ID_COLOUR, ID_TRACKS, ID_SEGMENT] {
            let mut out = Vec::new();
            write_id(id, &mut out);
            let (back, len) = read_id(&out, 0).unwrap();
            assert_eq!(back, id);
            assert_eq!(len, out.len());
        }
    }

    #[test]
    fn uints_pad_left_so_a_bigger_value_can_reuse_a_wider_field() {
        let mut out = Vec::new();
        write_uint_at(505, 4, &mut out).unwrap();
        assert_eq!(out, vec![0x00, 0x00, 0x01, 0xF9]);
        assert_eq!(min_uint_width(0), 1);
        assert_eq!(min_uint_width(505), 2);
    }

    #[test]
    fn floats_round_trip_at_the_width_matroska_uses() {
        let mut out = Vec::new();
        write_float_at(0.005, 4, &mut out).unwrap();
        let el = Elem {
            id: ID_LUMINANCE_MIN,
            id_len: 2,
            size: 4,
            size_len: 1,
            offset: 0,
            body: 0,
            unknown_size: false,
        };
        let back = read_float(&out, &el).unwrap();
        assert!((back - 0.005).abs() < 1e-9);
    }
}
