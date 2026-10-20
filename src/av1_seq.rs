//! The AV1 sequence header.
//!
//! Two things are wanted from it. `film_grain_params_present` says whether the
//! stream already carries a grain table, which has to come out before a new one
//! goes in. And almost every other field here is needed to parse a frame header
//! at all, because the frame header's shape depends on the sequence.
//!
//! The flag's exact bit position is reported, because writing it anywhere else
//! is a mistake that costs a stream: the last byte of a sequence header is the
//! trailing-bits byte, not the flag.

use anyhow::{bail, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, bit: 0 }
    }
    pub fn position(&self) -> usize {
        self.bit
    }
    pub fn f1(&mut self) -> Result<bool> {
        let byte = self.bit / 8;
        if byte >= self.data.len() {
            bail!("sequence header ended after {} bits", self.bit);
        }
        let shift = 7 - (self.bit % 8);
        self.bit += 1;
        Ok((self.data[byte] >> shift) & 1 == 1)
    }
    pub fn f(&mut self, n: usize) -> Result<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.f1()? as u64;
        }
        Ok(v)
    }
    /// Variable length code, as used for num_ticks_per_picture_minus_1.
    pub fn uvlc(&mut self) -> Result<u64> {
        let mut zeros = 0;
        while !self.f1()? {
            zeros += 1;
            if zeros > 32 {
                bail!("malformed uvlc in the sequence header");
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        Ok(self.f(zeros)? + (1u64 << zeros) - 1)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    #[allow(dead_code)]
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub frame_id_numbers_present: bool,
    pub use_128x128_superblock: bool,
    pub enable_order_hint: bool,
    pub order_hint_bits: u8,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub bit_depth: u8,
    pub mono_chrome: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub colour_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub film_grain_params_present: bool,
    /// Bit offset of `film_grain_params_present` inside the OBU payload.
    pub film_grain_flag_bit: usize,
    /// The timing the bitstream itself states, when it does: ticks per
    /// second, ticks per display unit, and ticks per picture when the
    /// interval is fixed. What a muxer derives the frame rate from.
    pub timing: Option<Timing>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub num_units_in_display_tick: u32,
    pub time_scale: u32,
    pub num_ticks_per_picture: Option<u32>,
}

impl Timing {
    /// Frames per second, when the interval is fixed.
    pub fn fps(&self) -> Option<f64> {
        let ticks = self.num_ticks_per_picture? as f64;
        if self.num_units_in_display_tick == 0 || ticks == 0.0 {
            return None;
        }
        Some(self.time_scale as f64 / (self.num_units_in_display_tick as f64 * ticks))
    }
}

impl SequenceHeader {
    /// Byte and bit-within-byte of the film grain flag, for a targeted rewrite.
    pub fn film_grain_flag_at(&self) -> (usize, u8) {
        (
            self.film_grain_flag_bit / 8,
            7 - (self.film_grain_flag_bit % 8) as u8,
        )
    }
}

/// Parse a sequence header OBU payload (the bytes after the OBU size field).
pub fn parse(payload: &[u8]) -> Result<SequenceHeader> {
    let mut r = BitReader::new(payload);
    let mut s = SequenceHeader {
        seq_profile: r.f(3)? as u8,
        still_picture: r.f1()?,
        ..Default::default()
    };
    s.reduced_still_picture_header = r.f1()?;

    let mut decoder_model_info_present = false;
    let mut buffer_delay_length = 0usize;
    if s.reduced_still_picture_header {
        s.seq_level_idx = r.f(5)? as u8;
    } else {
        let timing_info_present = r.f1()?;
        if timing_info_present {
            // timing_info()
            let num_units_in_display_tick = r.f(32)? as u32;
            let time_scale = r.f(32)? as u32;
            let num_ticks_per_picture = if r.f1()? {
                Some(r.uvlc()? as u32 + 1)
            } else {
                None
            };
            s.timing = Some(Timing {
                num_units_in_display_tick,
                time_scale,
                num_ticks_per_picture,
            });
            decoder_model_info_present = r.f1()?;
            if decoder_model_info_present {
                buffer_delay_length = r.f(5)? as usize + 1;
                r.f(32)?; // num_units_in_decoding_tick
                r.f(5)?; // buffer_removal_time_length_minus_1
                r.f(5)?; // frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay_present = r.f1()?;
        let operating_points = r.f(5)? as usize + 1;
        for i in 0..operating_points {
            r.f(12)?; // operating_point_idc
            let level = r.f(5)? as u8;
            let tier = if level > 7 { r.f(1)? as u8 } else { 0 };
            if i == 0 {
                s.seq_level_idx = level;
                s.seq_tier = tier;
            }
            if decoder_model_info_present && r.f1()? {
                // operating_parameters_info()
                r.f(buffer_delay_length)?;
                r.f(buffer_delay_length)?;
                r.f1()?; // low_delay_mode_flag
            }
            if initial_display_delay_present && r.f1()? {
                r.f(4)?; // initial_display_delay_minus_1
            }
        }
    }

    let width_bits = r.f(4)? as usize + 1;
    let height_bits = r.f(4)? as usize + 1;
    s.max_frame_width = r.f(width_bits)? as u32 + 1;
    s.max_frame_height = r.f(height_bits)? as u32 + 1;

    if !s.reduced_still_picture_header {
        s.frame_id_numbers_present = r.f1()?;
    }
    if s.frame_id_numbers_present {
        r.f(4)?; // delta_frame_id_length_minus_2
        r.f(3)?; // additional_frame_id_length_minus_1
    }
    s.use_128x128_superblock = r.f1()?;
    r.f1()?; // enable_filter_intra
    r.f1()?; // enable_intra_edge_filter

    if !s.reduced_still_picture_header {
        r.f1()?; // enable_interintra_compound
        r.f1()?; // enable_masked_compound
        r.f1()?; // enable_warped_motion
        r.f1()?; // enable_dual_filter
        s.enable_order_hint = r.f1()?;
        if s.enable_order_hint {
            r.f1()?; // enable_jnt_comp
            r.f1()?; // enable_ref_frame_mvs
        }
        let force_screen_content = if r.f1()? { 2 } else { r.f(1)? };
        if force_screen_content > 0 && !r.f1()? {
            r.f(1)?; // seq_force_integer_mv
        }
        if s.enable_order_hint {
            s.order_hint_bits = r.f(3)? as u8 + 1;
        }
    }
    s.enable_superres = r.f1()?;
    s.enable_cdef = r.f1()?;
    s.enable_restoration = r.f1()?;
    colour_config(&mut r, &mut s)?;
    s.film_grain_flag_bit = r.position();
    s.film_grain_params_present = r.f1()?;
    Ok(s)
}

fn colour_config(r: &mut BitReader, s: &mut SequenceHeader) -> Result<()> {
    let high_bitdepth = r.f1()?;
    s.bit_depth = if s.seq_profile == 2 && high_bitdepth {
        if r.f1()? {
            12
        } else {
            10
        }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    s.mono_chrome = if s.seq_profile == 1 { false } else { r.f1()? };

    if r.f1()? {
        s.colour_primaries = r.f(8)? as u8;
        s.transfer_characteristics = r.f(8)? as u8;
        s.matrix_coefficients = r.f(8)? as u8;
    } else {
        s.colour_primaries = 2; // unspecified
        s.transfer_characteristics = 2;
        s.matrix_coefficients = 2;
    }

    if s.mono_chrome {
        s.colour_range = r.f1()?;
        s.subsampling_x = true;
        s.subsampling_y = true;
        return Ok(());
    }
    // sRGB in an identity matrix is the one full-range case with no flag.
    if s.colour_primaries == 1 && s.transfer_characteristics == 13 && s.matrix_coefficients == 0 {
        s.colour_range = true;
    } else {
        s.colour_range = r.f1()?;
        match s.seq_profile {
            0 => {
                s.subsampling_x = true;
                s.subsampling_y = true;
            }
            1 => {}
            _ => {
                if s.bit_depth == 12 {
                    s.subsampling_x = r.f1()?;
                    s.subsampling_y = if s.subsampling_x { r.f1()? } else { false };
                } else {
                    s.subsampling_x = true;
                }
            }
        }
        if s.subsampling_x && s.subsampling_y {
            s.chroma_sample_position = r.f(2)? as u8;
        }
    }
    r.f1()?; // separate_uv_delta_q
    Ok(())
}

/// Pull the sequence header OBU payload out of an AV1CodecConfigurationRecord.
///
/// Matroska keeps it in CodecPrivate, so the sequence can be read without
/// touching a single Cluster.
pub fn from_codec_private(codec_private: &[u8]) -> Result<(SequenceHeader, usize)> {
    if codec_private.len() < 5 || codec_private[0] & 0x80 == 0 {
        bail!("CodecPrivate is not an AV1CodecConfigurationRecord");
    }
    let obus = &codec_private[4..];
    let mut pos = 0usize;
    while pos < obus.len() {
        let header = obus[pos];
        let obu_type = (header >> 3) & 0x0F;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut p = pos + 1;
        if extension {
            p += 1;
        }
        let (len, start) = if has_size {
            let mut value = 0u64;
            let mut n = 0;
            loop {
                let b = *obus.get(p + n).ok_or_else(|| {
                    anyhow::anyhow!("CodecPrivate ends inside an OBU size field")
                })?;
                value |= ((b & 0x7F) as u64) << (n * 7);
                n += 1;
                if b & 0x80 == 0 {
                    break;
                }
            }
            (value as usize, p + n)
        } else {
            (obus.len() - p, p)
        };
        let end = start + len;
        if end > obus.len() {
            bail!("CodecPrivate ends inside an OBU payload");
        }
        if obu_type == 1 {
            // The offset is relative to CodecPrivate, so a caller can rewrite
            // the flag where it actually lives.
            return Ok((parse(&obus[start..end])?, 4 + start));
        }
        pos = end;
    }
    bail!("CodecPrivate holds no sequence header OBU")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequence header of the 4K AV1 film this was built against, taken
    /// verbatim from its CodecPrivate.
    const CODEC_PRIVATE: [u8; 21] = [
        0x81, 0x0c, 0x4e, 0x00, 0x0a, 0x0f, 0x00, 0x00, 0x00, 0x62, 0xef, 0xbf, 0xe1, 0xbc, 0x02,
        0xf8, 0x50, 0x91, 0x00, 0x94, 0x40,
    ];

    #[test]
    fn reads_a_real_sequence_header_field_for_field() {
        let (s, offset) = from_codec_private(&CODEC_PRIVATE).unwrap();
        assert_eq!(offset, 6, "payload starts after the record and OBU headers");
        assert_eq!(s.seq_profile, 0);
        assert_eq!(s.seq_level_idx, 12); // level 5.0
        assert_eq!(s.seq_tier, 0);
        assert_eq!((s.max_frame_width, s.max_frame_height), (3840, 2160));
        assert_eq!(s.bit_depth, 10);
        assert!(!s.mono_chrome);
        assert_eq!(s.colour_primaries, 9); // BT.2020
        assert_eq!(s.transfer_characteristics, 16); // PQ
        assert_eq!(s.matrix_coefficients, 9); // BT.2020 non-constant
        assert!(!s.colour_range, "limited range");
        assert!(s.subsampling_x && s.subsampling_y, "4:2:0");
        assert_eq!(s.chroma_sample_position, 2);
        assert_eq!(s.order_hint_bits, 7);
    }

    #[test]
    fn finds_the_film_grain_flag_and_says_this_stream_has_none() {
        let (s, _) = from_codec_private(&CODEC_PRIVATE).unwrap();
        assert!(!s.film_grain_params_present);
        assert_eq!(s.film_grain_flag_bit, 112);
        assert_eq!(s.film_grain_flag_at(), (14, 7));
    }

    #[test]
    fn the_parse_lands_exactly_on_the_trailing_bits() {
        // The byte holding the flag must be the payload's last byte, and what
        // follows the flag must be trailing_one_bit and padding. Getting this
        // wrong is how a rewriter ends up toggling the trailing byte instead.
        let payload = &CODEC_PRIVATE[6..];
        let (s, _) = from_codec_private(&CODEC_PRIVATE).unwrap();
        let (byte, _) = s.film_grain_flag_at();
        assert_eq!(byte, payload.len() - 1);
        assert_eq!(payload[byte], 0x40, "flag 0, then trailing_one_bit");
    }

    #[test]
    fn setting_the_flag_touches_only_the_bit_it_should() {
        let (s, offset) = from_codec_private(&CODEC_PRIVATE).unwrap();
        let (byte, shift) = s.film_grain_flag_at();
        let mut patched = CODEC_PRIVATE;
        patched[offset + byte] |= 1 << shift;

        // 0x40 becomes 0xC0: the flag set, the trailing bits untouched.
        assert_eq!(patched[offset + byte], 0xC0);
        let (again, _) = from_codec_private(&patched).unwrap();
        assert!(again.film_grain_params_present);
        // Nothing else moved.
        assert_eq!(again.max_frame_width, s.max_frame_width);
        assert_eq!(again.transfer_characteristics, s.transfer_characteristics);
    }

    #[test]
    fn a_truncated_header_is_an_error_rather_than_a_guess() {
        assert!(parse(&[0x00, 0x00]).is_err());
        assert!(from_codec_private(&[0x81, 0x0c]).is_err());
    }
}
