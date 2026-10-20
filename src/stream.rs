//! Stream-level HDR metadata: decode it, re-encode it, write it back in place.
//!
//! AV1 and HEVC store the same numbers in different fixed-point formats and, for
//! the primaries, in a different order. Both payloads are fixed size, so a value
//! can be corrected by overwriting the bytes where they already are.

use anyhow::{bail, Result};
use serde::Serialize;

use crate::colour::Mdcv;
use crate::mkv::Codec;

/// Content light level, the same two numbers in both codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Cll {
    pub max_cll: u16,
    pub max_fall: u16,
}

pub fn decode_cll(payload: &[u8]) -> Result<Cll> {
    if payload.len() < 4 {
        bail!("content light level payload is {} bytes, need 4", payload.len());
    }
    Ok(Cll {
        max_cll: u16::from_be_bytes([payload[0], payload[1]]),
        max_fall: u16::from_be_bytes([payload[2], payload[3]]),
    })
}

pub fn encode_cll(v: Cll) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&v.max_cll.to_be_bytes());
    out[2..].copy_from_slice(&v.max_fall.to_be_bytes());
    out
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Decode a 24 byte mastering display payload.
///
/// AV1 stores chromaticities over 65536, luminance as 24.8 and 18.14 fixed
/// point, and lists the primaries red first. HEVC stores chromaticities over
/// 50000, both luminances in ten-thousandths of a nit, and lists the primaries
/// green first.
pub fn decode_mdcv(payload: &[u8], codec: Codec) -> Result<Mdcv> {
    if payload.len() < 24 {
        bail!(
            "mastering display payload is {} bytes, need 24",
            payload.len()
        );
    }
    let mut xy = [(0.0, 0.0); 3];
    match codec {
        Codec::Av1 => {
            for i in 0..3 {
                xy[i] = (
                    u16_at(payload, i * 4) as f64 / 65536.0,
                    u16_at(payload, i * 4 + 2) as f64 / 65536.0,
                );
            }
            Ok(Mdcv {
                primaries: xy,
                white_point: (
                    u16_at(payload, 12) as f64 / 65536.0,
                    u16_at(payload, 14) as f64 / 65536.0,
                ),
                luminance_max: u32_at(payload, 16) as f64 / 256.0,
                luminance_min: u32_at(payload, 20) as f64 / 16384.0,
            })
        }
        _ => {
            // Stored G, B, R - reorder to R, G, B.
            let g = (
                u16_at(payload, 0) as f64 / 50000.0,
                u16_at(payload, 2) as f64 / 50000.0,
            );
            let b = (
                u16_at(payload, 4) as f64 / 50000.0,
                u16_at(payload, 6) as f64 / 50000.0,
            );
            let r = (
                u16_at(payload, 8) as f64 / 50000.0,
                u16_at(payload, 10) as f64 / 50000.0,
            );
            Ok(Mdcv {
                primaries: [r, g, b],
                white_point: (
                    u16_at(payload, 12) as f64 / 50000.0,
                    u16_at(payload, 14) as f64 / 50000.0,
                ),
                luminance_max: u32_at(payload, 16) as f64 / 10000.0,
                luminance_min: u32_at(payload, 20) as f64 / 10000.0,
            })
        }
    }
}

fn round_u16(v: f64, scale: f64) -> Result<u16> {
    let n = (v * scale).round();
    if !(0.0..=65535.0).contains(&n) {
        bail!("chromaticity {v} is outside the range this format can store");
    }
    Ok(n as u16)
}

fn round_u32(v: f64, scale: f64) -> Result<u32> {
    let n = (v * scale).round();
    if !(0.0..=4_294_967_295.0).contains(&n) {
        bail!("luminance {v} is outside the range this format can store");
    }
    Ok(n as u32)
}

pub fn encode_mdcv(m: &Mdcv, codec: Codec) -> Result<[u8; 24]> {
    let mut out = [0u8; 24];
    match codec {
        Codec::Av1 => {
            for i in 0..3 {
                out[i * 4..i * 4 + 2]
                    .copy_from_slice(&round_u16(m.primaries[i].0, 65536.0)?.to_be_bytes());
                out[i * 4 + 2..i * 4 + 4]
                    .copy_from_slice(&round_u16(m.primaries[i].1, 65536.0)?.to_be_bytes());
            }
            out[12..14].copy_from_slice(&round_u16(m.white_point.0, 65536.0)?.to_be_bytes());
            out[14..16].copy_from_slice(&round_u16(m.white_point.1, 65536.0)?.to_be_bytes());
            out[16..20].copy_from_slice(&round_u32(m.luminance_max, 256.0)?.to_be_bytes());
            out[20..24].copy_from_slice(&round_u32(m.luminance_min, 16384.0)?.to_be_bytes());
        }
        _ => {
            let order = [m.primaries[1], m.primaries[2], m.primaries[0]]; // G, B, R
            for (i, p) in order.iter().enumerate() {
                out[i * 4..i * 4 + 2].copy_from_slice(&round_u16(p.0, 50000.0)?.to_be_bytes());
                out[i * 4 + 2..i * 4 + 4].copy_from_slice(&round_u16(p.1, 50000.0)?.to_be_bytes());
            }
            out[12..14].copy_from_slice(&round_u16(m.white_point.0, 50000.0)?.to_be_bytes());
            out[14..16].copy_from_slice(&round_u16(m.white_point.1, 50000.0)?.to_be_bytes());
            out[16..20].copy_from_slice(&round_u32(m.luminance_max, 10000.0)?.to_be_bytes());
            out[20..24].copy_from_slice(&round_u32(m.luminance_min, 10000.0)?.to_be_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bt2020() -> Mdcv {
        Mdcv {
            primaries: [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)],
            white_point: (0.3127, 0.3290),
            luminance_max: 1000.0,
            luminance_min: 0.005,
        }
    }

    #[test]
    fn av1_mastering_display_survives_a_round_trip() {
        let m = bt2020();
        let bytes = encode_mdcv(&m, Codec::Av1).unwrap();
        let back = decode_mdcv(&bytes, Codec::Av1).unwrap();
        assert!((back.luminance_max - 1000.0).abs() < 0.01);
        assert!((back.luminance_min - 0.005).abs() < 1e-4);
        for i in 0..3 {
            assert!((back.primaries[i].0 - m.primaries[i].0).abs() < 1e-4);
            assert!((back.primaries[i].1 - m.primaries[i].1).abs() < 1e-4);
        }
    }

    #[test]
    fn hevc_mastering_display_survives_a_round_trip_including_the_primary_order() {
        let m = bt2020();
        let bytes = encode_mdcv(&m, Codec::Hevc).unwrap();
        // Green is stored first in HEVC: 0.170 * 50000 = 8500.
        assert_eq!(u16_at(&bytes, 0), 8500);
        // Red comes third: 0.708 * 50000 = 35400.
        assert_eq!(u16_at(&bytes, 8), 35400);
        let back = decode_mdcv(&bytes, Codec::Hevc).unwrap();
        assert!((back.primaries[0].0 - 0.708).abs() < 1e-4);
        assert!((back.primaries[1].0 - 0.170).abs() < 1e-4);
        assert!((back.luminance_min - 0.005).abs() < 1e-6);
    }

    #[test]
    fn the_two_codecs_really_do_disagree_on_the_bytes() {
        let m = bt2020();
        let a = encode_mdcv(&m, Codec::Av1).unwrap();
        let h = encode_mdcv(&m, Codec::Hevc).unwrap();
        assert_ne!(a, h, "different scales and primary order must not coincide");
        // 1000 nits: 256000 in AV1's 24.8, 10000000 in HEVC's ten-thousandths.
        assert_eq!(u32_at(&a, 16), 256_000);
        assert_eq!(u32_at(&h, 16), 10_000_000);
    }

    #[test]
    fn the_files_own_luminance_min_decodes_to_what_ffprobe_reported() {
        // 2/16384 is what the AV1 stream in the test file carries.
        let mut payload = [0u8; 24];
        payload[16..20].copy_from_slice(&256_000u32.to_be_bytes());
        payload[20..24].copy_from_slice(&2u32.to_be_bytes());
        let m = decode_mdcv(&payload, Codec::Av1).unwrap();
        assert!((m.luminance_max - 1000.0).abs() < 1e-9);
        assert!((m.luminance_min - 0.000122).abs() < 1e-6);
    }

    #[test]
    fn content_light_level_round_trips() {
        let v = Cll {
            max_cll: 1768,
            max_fall: 505,
        };
        assert_eq!(decode_cll(&encode_cll(v)).unwrap(), v);
        assert!(decode_cll(&[0, 1]).is_err());
    }
}
