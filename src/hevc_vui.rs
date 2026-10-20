//! The colour an HEVC stream states about itself.
//!
//! The VUI at the end of the sequence parameter set says the primaries, the
//! transfer and the matrix - what a decoder goes by, and what decides whether
//! a Dolby Vision base layer is HDR10, HLG or SDR compatible. Getting there
//! means walking the whole SPS, short-term reference picture sets included.
//! The walk follows hevc_parser (MIT, quietvoid), which keeps its SPS fields
//! to itself.

use crate::hevc;

/// What the VUI says about the picture, as H.273 code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Signal {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

const NAL_SPS: u8 = 33;

/// The first SPS of an hvcC record - Matroska's CodecPrivate - read for its
/// VUI. None when there is no SPS, or it states no colour.
pub fn from_hvcc(record: &[u8]) -> Option<Signal> {
    let arrays = *record.get(22)? as usize;
    let mut at = 23;
    for _ in 0..arrays {
        let nal_type = record.get(at)? & 0x3F;
        let count = u16::from_be_bytes([*record.get(at + 1)?, *record.get(at + 2)?]) as usize;
        at += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes([*record.get(at)?, *record.get(at + 1)?]) as usize;
            at += 2;
            let nal = record.get(at..at + len)?;
            at += len;
            if nal_type == NAL_SPS {
                return from_sps(nal);
            }
        }
    }
    None
}

/// The VUI colour of one SPS NAL unit, its two header bytes included.
pub fn from_sps(nal: &[u8]) -> Option<Signal> {
    if nal.len() < 3 || (nal[0] >> 1) & 0x3F != NAL_SPS {
        return None;
    }
    let rbsp = hevc::unescape(&nal[2..]);
    walk(&mut Bits { data: &rbsp, pos: 0 })
}

fn walk(r: &mut Bits) -> Option<Signal> {
    r.skip(4)?; // sps_video_parameter_set_id
    let sub_layers_minus1 = r.u(3)? as usize;
    r.skip(1)?; // sps_temporal_id_nesting_flag
    profile_tier_level(r, sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    if r.ue()? == 3 {
        r.skip(1)?; // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?; // conformance window offsets
        }
    }
    r.ue()?; // bit_depth_luma_minus8
    r.ue()?; // bit_depth_chroma_minus8
    let poc_lsb_bits = r.ue()? + 4;
    let first = if r.flag()? { 0 } else { sub_layers_minus1 };
    for _ in first..=sub_layers_minus1 {
        r.ue()?; // max_dec_pic_buffering_minus1
        r.ue()?; // max_num_reorder_pics
        r.ue()?; // max_latency_increase_plus1
    }
    for _ in 0..6 {
        r.ue()?; // block sizes, transform sizes, hierarchy depths
    }
    if r.flag()? && r.flag()? {
        scaling_list_data(r)?;
    }
    r.skip(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if r.flag()? {
        r.skip(8)?; // pcm sample bit depths
        r.ue()?;
        r.ue()?;
        r.skip(1)?; // pcm_loop_filter_disabled_flag
    }
    let sets = r.ue()? as usize;
    if sets > 64 {
        return None;
    }
    // Each set's number of delta POCs; a predicted set lists one flag (or
    // two) per entry of the set before it, plus one.
    let mut deltas: Vec<usize> = Vec::with_capacity(sets);
    for i in 0..sets {
        if i > 0 && r.flag()? {
            r.skip(1)?; // delta_rps_sign
            r.ue()?; // abs_delta_rps_minus1
            let mut n = 0;
            for _ in 0..=deltas[i - 1] {
                let used = r.flag()?;
                if used || r.flag()? {
                    n += 1;
                }
            }
            deltas.push(n);
        } else {
            let pictures = r.ue()? as usize + r.ue()? as usize;
            if pictures > 32 {
                return None;
            }
            for _ in 0..pictures {
                r.ue()?; // delta_poc_minus1
                r.skip(1)?; // used_by_curr_pic_flag
            }
            deltas.push(pictures);
        }
    }
    if r.flag()? {
        let long_term = r.ue()?;
        if long_term > 32 {
            return None;
        }
        for _ in 0..long_term {
            r.skip(poc_lsb_bits as usize + 1)?;
        }
    }
    r.skip(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    if !r.flag()? {
        return None; // no VUI
    }
    if r.flag()? && r.u(8)? == 255 {
        r.skip(32)?; // sar_width, sar_height
    }
    if r.flag()? {
        r.skip(1)?; // overscan_appropriate_flag
    }
    if !r.flag()? {
        return None; // no video signal type
    }
    r.skip(3)?; // video_format
    let full_range = r.flag()?;
    if !r.flag()? {
        return None; // no colour description
    }
    Some(Signal {
        primaries: r.u(8)? as u8,
        transfer: r.u(8)? as u8,
        matrix: r.u(8)? as u8,
        full_range,
    })
}

fn profile_tier_level(r: &mut Bits, sub_layers_minus1: usize) -> Option<()> {
    // general profile space, tier and idc, 32 compatibility flags, 48 bits
    // of source and constraint flags, level
    r.skip(8 + 32 + 48 + 8)?;
    let mut profile = [false; 8];
    let mut level = [false; 8];
    for i in 0..sub_layers_minus1 {
        profile[i] = r.flag()?;
        level[i] = r.flag()?;
    }
    if sub_layers_minus1 > 0 {
        r.skip(2 * (8 - sub_layers_minus1))?; // reserved_zero_2bits
    }
    for i in 0..sub_layers_minus1 {
        if profile[i] {
            r.skip(8 + 32 + 48)?;
        }
        if level[i] {
            r.skip(8)?;
        }
    }
    Some(())
}

fn scaling_list_data(r: &mut Bits) -> Option<()> {
    for size_id in 0..4usize {
        let matrices = if size_id == 3 { 2 } else { 6 };
        for _ in 0..matrices {
            if !r.flag()? {
                r.ue()?; // scaling_list_pred_matrix_id_delta
            } else {
                if size_id > 1 {
                    r.se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..64.min(1 << (4 + (size_id << 1))) {
                    r.se()?; // scaling_list_delta_coef
                }
            }
        }
    }
    Some(())
}

/// Bits, most significant first, and Exp-Golomb codes. Running off the end
/// is None, never a panic: a damaged SPS just says nothing.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u64> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u64)
    }

    fn u(&mut self, n: usize) -> Option<u64> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes an SPS the way the syntax lays it out.
    #[derive(Default)]
    struct W {
        bits: Vec<bool>,
    }

    impl W {
        fn u(&mut self, n: usize, v: u64) -> &mut Self {
            for i in (0..n).rev() {
                self.bits.push((v >> i) & 1 == 1);
            }
            self
        }
        fn f(&mut self, b: bool) -> &mut Self {
            self.bits.push(b);
            self
        }
        fn ue(&mut self, v: u64) -> &mut Self {
            let x = v + 1;
            let len = 64 - x.leading_zeros() as usize;
            self.u(len - 1, 0).u(len, x)
        }
        fn se(&mut self, v: i64) -> &mut Self {
            self.ue(if v > 0 { (2 * v - 1) as u64 } else { (-2 * v) as u64 })
        }
        fn nal(&self) -> Vec<u8> {
            let mut bytes: Vec<u8> = self
                .bits
                .chunks(8)
                .map(|c| c.iter().enumerate().fold(0u8, |a, (i, b)| a | (*b as u8) << (7 - i)))
                .collect();
            bytes.push(0x80); // rbsp trailing bits
            let mut nal = vec![NAL_SPS << 1, 0x01];
            nal.extend(hevc::escape(&bytes));
            nal
        }
    }

    /// An SPS with everything that moves the VUI: sub-layers with their own
    /// profile and level, a scaling list, PCM, a predicted reference picture
    /// set, long-term pictures and an extended sample aspect ratio.
    fn rich_sps(transfer: u8) -> Vec<u8> {
        let mut w = W::default();
        w.u(4, 0).u(3, 2).f(true); // vps id, 3 sub-layers, nesting
        // profile_tier_level: general part, then two sub-layers
        w.u(8, 0x02).u(32, 0x2000_0000).u(48, 0x9000_0000_0000).u(8, 153);
        w.f(true).f(true).f(false).f(true); // sub-layer 0: profile+level; 1: level
        w.u(2 * 6, 0); // reserved for the unused sub-layer slots
        w.u(40, 0).u(48, 0x5a5a).u(8, 120); // sub-layer 0 profile (88 bits), level
        w.u(8, 90); // sub-layer 1 level
        w.ue(0).ue(1); // sps id, 4:2:0
        w.ue(3840).ue(2160).f(true).ue(0).ue(0).ue(0).ue(4); // size, conformance window
        w.ue(2).ue(2).ue(4); // 10 bit, 10 bit, poc lsb 8 bits
        w.f(true); // ordering info for every sub-layer
        for _ in 0..3 {
            w.ue(4).ue(2).ue(0);
        }
        w.ue(0).ue(3).ue(0).ue(3).ue(1).ue(1); // block and transform sizes
        // scaling list: enabled, present, every kind of entry
        w.f(true).f(true);
        for size_id in 0..4 {
            let matrices = if size_id == 3 { 2 } else { 6 };
            for m in 0..matrices {
                if m % 2 == 0 {
                    w.f(false).ue(0);
                } else {
                    w.f(true);
                    if size_id > 1 {
                        w.se(-3);
                    }
                    for i in 0..64.min(1 << (4 + (size_id << 1))) {
                        w.se(if i % 3 == 0 { 2 } else { -1 });
                    }
                }
            }
        }
        w.f(true).f(true); // amp, sao
        w.f(true).u(4, 7).u(4, 7).ue(0).ue(1).f(false); // pcm
        // three reference picture sets: explicit, predicted, explicit
        w.ue(3);
        w.ue(2).ue(1).ue(0).f(true).ue(1).f(true).ue(0).f(false);
        w.f(true).f(false).ue(0); // predicted from set 0 (3 entries + 1)
        w.f(true).f(false).f(true).f(false).f(false).f(true);
        w.f(false).ue(1).ue(0).ue(3).f(true);
        w.f(true).ue(2).u(8, 17).f(true).u(8, 33).f(false); // long-term pictures
        w.f(true).f(true); // temporal mvp, strong intra smoothing
        // VUI
        w.f(true);
        w.f(true).u(8, 255).u(16, 4).u(16, 3); // extended SAR
        w.f(true).f(false); // overscan info
        w.f(true).u(3, 5).f(false).f(true).u(8, 9).u(8, transfer as u64).u(8, 9);
        w.f(false).f(false).f(false).f(false); // the rest of the VUI, unread
        w.nal()
    }

    #[test]
    fn the_colour_is_found_behind_everything_an_sps_can_hold() {
        for transfer in [16, 18, 14, 1] {
            let s = from_sps(&rich_sps(transfer)).expect("a VUI");
            assert_eq!(s, Signal { primaries: 9, transfer, matrix: 9, full_range: false });
        }
    }

    #[test]
    fn an_hvcc_record_gives_its_first_sps() {
        let sps = rich_sps(18);
        let mut record = vec![0u8; 22];
        record[0] = 1;
        record.push(2); // two arrays: a VPS, then the SPS
        record.extend_from_slice(&[0x20, 0, 1, 0, 3, 0x40, 0x01, 0xAA]);
        record.extend_from_slice(&[0x21, 0, 1]);
        record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        record.extend_from_slice(&sps);
        assert_eq!(from_hvcc(&record).map(|s| s.transfer), Some(18));
    }

    #[test]
    fn a_short_or_silent_sps_says_nothing() {
        let sps = rich_sps(16);
        for cut in [3, 10, sps.len() / 2, sps.len() - 6] {
            assert_eq!(from_sps(&sps[..cut]), None, "cut at {cut}");
        }
        assert_eq!(from_sps(&[0x40, 0x01, 0xAA]), None); // a VPS
        assert_eq!(from_hvcc(&[1, 2, 3]), None);
    }
}
