//! HEVC access units the way Matroska stores them: length-prefixed NAL units.
//!
//! The same metadata AV1 keeps in OBUs lives here in SEI messages and, for
//! Dolby Vision, in an unspecified NAL type 62. Two things make HEVC fussier
//! than AV1: SEI payloads are wrapped in emulation prevention bytes, so what
//! is on disk is not the value; and one SEI NAL may carry several messages,
//! so removing one means rebuilding the NAL around the rest.

use anyhow::{bail, Context, Result};

pub const NAL_EOS: u8 = 36;
pub const NAL_EOB: u8 = 37;
pub const NAL_SEI_PREFIX: u8 = 39;
pub const NAL_UNSPEC62: u8 = 62;

pub const SEI_T35: u8 = 4;
pub const SEI_MDCV: u8 = 137;
pub const SEI_CLL: u8 = 144;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Other,
    Rpu,
    Hdr10Plus,
    Cll,
    Mdcv,
}

/// One SEI message, payload unescaped.
#[derive(Clone, Debug)]
pub struct SeiMessage {
    pub payload_type: u32,
    pub payload: Vec<u8>,
}

impl SeiMessage {
    pub fn unit(&self) -> Unit {
        match self.payload_type {
            x if x == SEI_MDCV as u32 => Unit::Mdcv,
            x if x == SEI_CLL as u32 => Unit::Cll,
            x if x == SEI_T35 as u32 && self.payload.len() >= 3 && self.payload[0] == 0xB5 => {
                match u16::from_be_bytes([self.payload[1], self.payload[2]]) {
                    0x003C => Unit::Hdr10Plus,
                    0x003B => Unit::Rpu,
                    _ => Unit::Other,
                }
            }
            _ => Unit::Other,
        }
    }
}

pub struct NalRef {
    /// Range of the NAL including its length prefix.
    pub start: usize,
    pub end: usize,
    pub nal_type: u8,
    /// For a prefix SEI: its messages. Empty for every other NAL type.
    pub messages: Vec<SeiMessage>,
}

impl NalRef {
    pub fn is_vcl(&self) -> bool {
        self.nal_type < 32
    }
    /// What a whole NAL is, when it is one thing. Stripping works message by
    /// message and does not need this; the tests and a future report do.
    #[allow(dead_code)]
    pub fn unit(&self) -> Unit {
        if self.nal_type == NAL_UNSPEC62 {
            return Unit::Rpu;
        }
        // A NAL with one message is classified by it; mixed NALs are handled
        // message by message when stripping.
        match self.messages.as_slice() {
            [only] => only.unit(),
            _ => Unit::Other,
        }
    }
}

/// Remove emulation prevention bytes.
pub fn unescape(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut zeros = 0;
    for &b in src {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Insert emulation prevention bytes: no 00 00 0x with x <= 3 may appear.
pub fn escape(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 64 + 2);
    let mut zeros = 0;
    for &b in src {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Parse the messages of a prefix SEI RBSP (unescaped, after the NAL header).
fn parse_sei(rbsp: &[u8]) -> Vec<SeiMessage> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < rbsp.len() {
        // Trailing bits: 0x80 followed by nothing, or padding.
        if rbsp[pos..].iter().all(|&b| b == 0x80 || b == 0) {
            break;
        }
        let mut payload_type = 0u32;
        while let Some(&b) = rbsp.get(pos) {
            pos += 1;
            payload_type += b as u32;
            if b != 0xFF {
                break;
            }
        }
        let mut payload_size = 0usize;
        while let Some(&b) = rbsp.get(pos) {
            pos += 1;
            payload_size += b as usize;
            if b != 0xFF {
                break;
            }
        }
        let end = pos.saturating_add(payload_size);
        if end > rbsp.len() {
            break;
        }
        out.push(SeiMessage {
            payload_type,
            payload: rbsp[pos..end].to_vec(),
        });
        pos = end;
    }
    out
}

/// Split an access unit into its NAL units.
/// Whether this block is a length-prefixed HEVC access unit at all.
///
/// A video track is supposed to hold nothing else, but a botched remux can
/// leave blocks of a track that was removed sitting in it under the video's
/// own number - zlib-compressed subtitles, for one. They decode to nothing,
/// they share timestamps with real pictures, and no metadata belongs on
/// them. The walk is the proof: the lengths have to tile the block exactly.
pub fn is_access_unit(tu: &[u8], len_size: usize) -> bool {
    let mut pos = 0usize;
    let mut nals = 0usize;
    while pos < tu.len() {
        if pos + len_size > tu.len() {
            return false;
        }
        let mut len = 0usize;
        for i in 0..len_size {
            len = (len << 8) | tu[pos + i] as usize;
        }
        if len < 2 {
            return false;
        }
        match pos.checked_add(len_size + len) {
            Some(end) if end <= tu.len() => pos = end,
            _ => return false,
        }
        nals += 1;
    }
    nals > 0
}

pub fn split_nals(tu: &[u8], len_size: usize) -> Result<Vec<NalRef>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < tu.len() {
        if pos + len_size > tu.len() {
            bail!("access unit ends inside a NAL length prefix");
        }
        let mut len = 0usize;
        for i in 0..len_size {
            len = (len << 8) | tu[pos + i] as usize;
        }
        let start = pos + len_size;
        let end = start
            .checked_add(len)
            .filter(|e| *e <= tu.len())
            .context("NAL unit runs past the end of its access unit")?;
        if len < 2 {
            bail!("NAL unit shorter than its header");
        }
        let nal_type = (tu[start] >> 1) & 0x3F;
        let messages = if nal_type == NAL_SEI_PREFIX {
            parse_sei(&unescape(&tu[start + 2..end]))
        } else {
            Vec::new()
        };
        out.push(NalRef {
            start: pos,
            end,
            nal_type,
            messages,
        });
        pos = end;
    }
    Ok(out)
}

fn write_ff_coded(mut value: usize, out: &mut Vec<u8>) {
    while value >= 255 {
        out.push(0xFF);
        value -= 255;
    }
    out.push(value as u8);
}

/// Build a prefix SEI NAL (without length prefix) from messages.
pub fn sei_prefix_nal(messages: &[SeiMessage]) -> Vec<u8> {
    let mut rbsp = Vec::new();
    for m in messages {
        write_ff_coded(m.payload_type as usize, &mut rbsp);
        write_ff_coded(m.payload.len(), &mut rbsp);
        rbsp.extend_from_slice(&m.payload);
    }
    rbsp.push(0x80); // rbsp_trailing_bits
    let mut nal = vec![NAL_SEI_PREFIX << 1, 0x01]; // layer 0, temporal id 1
    nal.extend_from_slice(&escape(&rbsp));
    nal
}

/// The TemporalId (plus one) an access unit runs at, from its first slice.
/// Streams without temporal layers say 1 everywhere.
pub fn temporal_id_plus1(body: &[u8], nals: &[NalRef], len_size: usize) -> u8 {
    nals.iter()
        .find(|n| n.is_vcl())
        .and_then(|n| body.get(n.start + len_size + 1))
        .map(|b| b & 0x07)
        .filter(|t| *t != 0)
        .unwrap_or(1)
}

/// Give a length-prefixed NAL the TemporalId of the access unit it goes into.
/// A prefix SEI has to match its access unit there; the encoders write 1 and
/// are right for every stream without temporal layers.
pub fn set_temporal_id(len_size: usize, unit: &mut [u8], tid_plus1: u8) {
    if let Some(b) = unit.get_mut(len_size + 1) {
        *b = (*b & 0xF8) | (tid_plus1 & 0x07);
    }
}

/// A NAL with its length prefix in front, ready to go into a block.
pub fn with_length(len_size: usize, nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(len_size + nal.len());
    for i in (0..len_size).rev() {
        out.push((nal.len() >> (8 * i)) as u8);
    }
    out.extend_from_slice(nal);
    out
}

/// Where new prefix SEI NALs go: right before the first VCL NAL, after the
/// parameter sets and whatever SEI is already there.
pub fn prefix_insert_index(nals: &[NalRef]) -> usize {
    nals.iter()
        .find(|n| n.is_vcl())
        .map(|n| n.start)
        .unwrap_or_else(|| nals.last().map(|n| n.end).unwrap_or(0))
}

/// Where the RPU goes: last in the access unit, but ahead of any end-of-
/// sequence or end-of-bitstream marker, which nothing may follow.
pub fn rpu_insert_index(nals: &[NalRef], tu_len: usize) -> usize {
    let mut at = tu_len;
    for n in nals.iter().rev() {
        if n.nal_type == NAL_EOS || n.nal_type == NAL_EOB {
            at = n.start;
        } else {
            break;
        }
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check that tells a picture from whatever a botched remux left in
    /// the video track. Sven's file held zlib-compressed subtitle blocks
    /// under the video's own track number; they begin 78 da, and their
    /// first four bytes read as a NAL length of two billion.
    #[test]
    fn a_block_that_is_not_an_access_unit_is_seen_through() {
        let nal = |len: usize| {
            let mut v = (len as u32).to_be_bytes().to_vec();
            v.extend(std::iter::repeat_n(0x40u8, len));
            v
        };
        let mut good = nal(24);
        good.extend(nal(58));
        assert!(is_access_unit(&good, 4));

        // One byte short, one byte over: neither tiles the block.
        assert!(!is_access_unit(&good[..good.len() - 1], 4));
        let mut over = good.clone();
        over.push(0);
        assert!(!is_access_unit(&over, 4));

        // Real zlib, as the blocks in that file began.
        let zlib = [0x78, 0xda, 0x13, 0x63, 0xe0, 0x66, 0x6f, 0x60, 0xb1, 0x10];
        assert!(!is_access_unit(&zlib, 4));

        // Nothing at all, and a length that claims more than there is.
        assert!(!is_access_unit(&[], 4));
        assert!(!is_access_unit(&[0, 0, 0, 200, 0x40, 0x01], 4));
        // A NAL too short to hold its own two-byte header.
        assert!(!is_access_unit(&[0, 0, 0, 1, 0x40], 4));
        // Two-byte lengths are as valid a width as four.
        assert!(is_access_unit(&[0, 2, 0x40, 0x01], 2));
    }

    #[test]
    fn escaping_round_trips_and_protects_start_codes() {
        let raw = vec![0x00, 0x00, 0x01, 0xAA, 0x00, 0x00, 0x00, 0x00, 0x03, 0xFF];
        let esc = escape(&raw);
        // No 00 00 0x with x <= 2 survives; 00 00 03 is the guard itself.
        for w in esc.windows(3) {
            assert!(!(w[0] == 0 && w[1] == 0 && w[2] <= 2), "{esc:02x?}");
        }
        assert_eq!(unescape(&esc), raw);
        // The real world case: a content light level of 0/0 is four zero bytes.
        assert_eq!(escape(&[0, 0, 0, 0]), vec![0, 0, 3, 0, 0]);
    }

    #[test]
    fn a_sei_nal_is_parsed_back_into_its_messages() {
        let cll = SeiMessage {
            payload_type: SEI_CLL as u32,
            payload: vec![0x06, 0xE8, 0x01, 0xF9], // 1768 / 505
        };
        let mdcv = SeiMessage {
            payload_type: SEI_MDCV as u32,
            payload: vec![0u8; 24], // all zero: needs escaping on the way out
        };
        let nal = sei_prefix_nal(&[cll.clone(), mdcv.clone()]);
        let tu = with_length(4, &nal);
        let nals = split_nals(&tu, 4).unwrap();
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].nal_type, NAL_SEI_PREFIX);
        let m = &nals[0].messages;
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].unit(), Unit::Cll);
        assert_eq!(m[0].payload, cll.payload);
        assert_eq!(m[1].unit(), Unit::Mdcv);
        assert_eq!(m[1].payload, mdcv.payload);
        // Mixed NAL: classified as Other at NAL level.
        assert_eq!(nals[0].unit(), Unit::Other);
    }

    #[test]
    fn hdr10plus_and_rpu_are_told_apart() {
        let hp = sei_prefix_nal(&[SeiMessage {
            payload_type: 4,
            payload: vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04],
        }]);
        let mut tu = with_length(4, &hp);
        tu.extend_from_slice(&with_length(4, &[0x7C, 0x01, 0x19, 0x00]));
        tu.extend_from_slice(&with_length(4, &[NAL_EOS << 1, 0x01]));
        let nals = split_nals(&tu, 4).unwrap();
        assert_eq!(nals[0].unit(), Unit::Hdr10Plus);
        assert_eq!(nals[1].unit(), Unit::Rpu);
        // A new RPU belongs before the end-of-sequence marker.
        assert_eq!(rpu_insert_index(&nals, tu.len()), nals[2].start);
        assert_eq!(prefix_insert_index(&nals), tu.len(), "no VCL here");
    }

    #[test]
    fn prefix_sei_goes_before_the_first_slice() {
        let mut tu = with_length(4, &[35 << 1, 0x01, 0x50]); // AUD
        tu.extend_from_slice(&with_length(4, &[33 << 1, 0x01, 0xAA])); // SPS
        let slice_at = tu.len();
        tu.extend_from_slice(&with_length(4, &[19 << 1, 0x01, 0xAF, 0x00])); // IDR
        let nals = split_nals(&tu, 4).unwrap();
        assert_eq!(prefix_insert_index(&nals), slice_at);
        assert_eq!(rpu_insert_index(&nals, tu.len()), tu.len());
    }

    #[test]
    fn a_truncated_unit_is_an_error() {
        assert!(split_nals(&[0, 0, 0, 9, 0x4E], 4).is_err());
    }

    #[test]
    fn inserted_sei_take_the_temporal_id_of_their_access_unit() {
        // A TSA picture at TemporalId 2 (nuh_temporal_id_plus1 = 3).
        let mut tu = with_length(4, &[33 << 1, 0x01, 0xAA]); // SPS, tid 0
        tu.extend_from_slice(&with_length(4, &[2 << 1, 0x03, 0xAF, 0x00])); // TSA_N
        let nals = split_nals(&tu, 4).unwrap();
        assert_eq!(temporal_id_plus1(&tu, &nals, 4), 3);

        let mut sei = with_length(4, &sei_prefix_nal(&[SeiMessage {
            payload_type: SEI_CLL as u32,
            payload: vec![0x06, 0xE8, 0x01, 0xF9],
        }]));
        assert_eq!(sei[5], 0x01, "encoded at TemporalId 0");
        set_temporal_id(4, &mut sei, 3);
        assert_eq!(sei[5], 0x03);
        assert_eq!(sei[4], NAL_SEI_PREFIX << 1, "the type byte is untouched");
        // No slice at all: fall back to 1 rather than 0, which is forbidden.
        assert_eq!(temporal_id_plus1(&tu[..7], &split_nals(&tu[..7], 4).unwrap(), 4), 1);
    }
}
