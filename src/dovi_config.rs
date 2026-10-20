//! Dolby Vision signalling in the container.
//!
//! The RPUs inside the stream are what a decoder uses, but scanners and players
//! decide whether a file *is* Dolby Vision from the container: a
//! DOVIDecoderConfigurationRecord, carried in Matroska as a BlockAdditionMapping
//! of type `dvvC`. It is what MediaInfo turns into "dav1.10 / 06 / BL+RPU", and
//! a file without it reads as plain HDR10 no matter how many RPUs it holds.

use crate::ebml::{self, Elem};
use crate::mkv::{Codec, Head};

/// The 24 byte record, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoviConfig {
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    /// 1 = HDR10 compatible (x.1), 2 = SDR (x.2), 4 = HLG (x.4), 0 = none.
    pub bl_signal_compatibility_id: u8,
}

impl DoviConfig {
    pub fn encode(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0] = 1; // dv_version_major
        out[1] = 0; // dv_version_minor
        let mid: u16 = ((self.profile as u16 & 0x7F) << 9)
            | ((self.level as u16 & 0x3F) << 3)
            | ((self.rpu_present as u16) << 2)
            | ((self.el_present as u16) << 1)
            | (self.bl_present as u16);
        out[2..4].copy_from_slice(&mid.to_be_bytes());
        out[4] = (self.bl_signal_compatibility_id & 0x0F) << 4;
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<DoviConfig> {
        if bytes.len() < 5 || bytes[0] != 1 {
            return None;
        }
        let mid = u16::from_be_bytes([bytes[2], bytes[3]]);
        Some(DoviConfig {
            profile: (mid >> 9) as u8 & 0x7F,
            level: (mid >> 3) as u8 & 0x3F,
            rpu_present: mid & 0x04 != 0,
            el_present: mid & 0x02 != 0,
            bl_present: mid & 0x01 != 0,
            bl_signal_compatibility_id: bytes[4] >> 4,
        })
    }
}

/// The Dolby Vision level a stream needs, from its pixel rate.
///
/// Levels are defined by a maximum pixel rate (and a bitrate we cannot know
/// here); the smallest level whose rate covers the stream is the one to signal.
/// Levels 10 and 11 share a rate and differ only in picture size.
pub fn level_for(width: u32, height: u32, fps: f64) -> u8 {
    let pps = width as f64 * height as f64 * fps;
    const RATES: [(u8, f64); 13] = [
        (1, 22_118_400.0),    // 1280x720p24
        (2, 27_648_000.0),    // 1280x720p30
        (3, 49_766_400.0),    // 1920x1080p24
        (4, 62_208_000.0),    // 1920x1080p30
        (5, 124_416_000.0),   // 1920x1080p60
        (6, 199_065_600.0),   // 3840x2160p24
        (7, 248_832_000.0),   // 3840x2160p30
        (8, 398_131_200.0),   // 3840x2160p48
        (9, 497_664_000.0),   // 3840x2160p60
        (10, 995_328_000.0),  // 3840x2160p120
        (11, 995_328_000.0),  // 7680x4320p30
        (12, 1_990_656_000.0), // 7680x4320p60
        (13, 3_981_312_000.0), // 7680x4320p120
    ];
    for (level, rate) in RATES {
        if pps <= rate {
            if level == 10 && width > 3840 {
                continue;
            }
            return level;
        }
    }
    13
}

/// A compatibility id and the reason for it, for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compat {
    pub id: u8,
    pub why: &'static str,
    /// False when the picture does not say what it is and HDR10 was taken.
    pub sure: bool,
}

/// The compatibility id of the base layer: what a player without Dolby
/// Vision makes of the picture. Profile 5 RPUs belong with a picture of
/// their own (IPT) that only Dolby Vision can show, so none. Every other
/// profile rides on a plain picture, and its transfer (H.273) says which
/// kind: PQ is HDR10, HLG is HLG, a gamma curve is SDR. The same id goes
/// behind the profile for HEVC (8.1, 8.4) and AV1 (10.1, 10.4).
pub fn compat_for(rpu_profile: u8, transfer: Option<u8>) -> Compat {
    if rpu_profile == 5 {
        return Compat {
            id: 0,
            why: "profile 5 RPUs: a picture only Dolby Vision can show",
            sure: true,
        };
    }
    if rpu_profile == 7 {
        return Compat {
            id: 6,
            why: "profile 7 with its enhancement layer, as on a disc",
            sure: true,
        };
    }
    match transfer {
        Some(16) => Compat { id: 1, why: "the picture is PQ: HDR10 compatible", sure: true },
        Some(18) => Compat { id: 4, why: "the picture is HLG: HLG compatible", sure: true },
        Some(1 | 4 | 5 | 6 | 7 | 11 | 12 | 13 | 14 | 15) => Compat {
            id: 2,
            why: "the picture is SDR: SDR compatible",
            sure: true,
        },
        _ => Compat {
            id: 1,
            why: "the picture does not say what it is; HDR10 taken",
            sure: false,
        },
    }
}

/// The transfer the picture states (H.273): the stream first - the AV1
/// sequence header, the HEVC sequence parameter set - then the container.
/// 2 means "unspecified" and says nothing.
pub fn picture_transfer(head: &Head) -> Option<u8> {
    let stated = |t: u8| (t != 2).then_some(t);
    let record = head
        .video
        .codec_private
        .as_ref()
        .map(|cp| &head.buf[cp.body..cp.body_end()]);
    let stream = match head.video.codec {
        Codec::Av1 => record
            .and_then(|r| crate::av1_seq::from_codec_private(r).ok())
            .map(|(s, _)| s.transfer_characteristics),
        Codec::Hevc => record
            .and_then(crate::hevc_vui::from_hvcc)
            .map(|s| s.transfer),
        Codec::Other => None,
    };
    stream.and_then(stated).or_else(|| {
        crate::colour::read(&head.buf, head.video.colour.as_ref())
            .ok()
            .and_then(|c| c.transfer_characteristics)
            .and_then(|t| u8::try_from(t).ok())
            .and_then(stated)
    })
}

/// What speaks against an id somebody chose, if anything.
pub fn compat_doubt(rpu_profile: u8, id: u8, transfer: Option<u8>) -> Option<String> {
    let fits = compat_for(rpu_profile, transfer);
    if rpu_profile == 5 && id != 0 {
        return Some(format!(
            "profile 5 RPUs go with a picture no player without Dolby Vision can show; \
             their id is 0, not {id}"
        ));
    }
    if rpu_profile != 5 && id == 0 {
        return Some(format!(
            "0 says no player without Dolby Vision can show the picture, which only holds \
             for profile 5; these RPUs are profile {rpu_profile}"
        ));
    }
    if fits.sure && fits.id != id {
        return Some(format!("{}, which is id {}", fits.why, fits.id));
    }
    None
}

const ID_BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
const ID_BLOCK_ADD_ID_NAME: u32 = 0x41A4;
const ID_BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
const ID_BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;
const TYPE_DVVC: &[u8; 4] = b"dvvC";
const TYPE_DVCC: &[u8; 4] = b"dvcC";

/// The BlockAdditionMapping element carrying a record, laid out the way
/// mkvmerge writes it.
pub fn mapping_element(record: &[u8; 24], legacy: bool) -> Vec<u8> {
    let mut body = Vec::new();
    emit(ID_BLOCK_ADD_ID_NAME, b"Dolby Vision configuration", &mut body);
    // dvcC is for profiles up to 7; everything newer, AV1 included, is dvvC.
    emit(
        ID_BLOCK_ADD_ID_TYPE,
        if legacy { TYPE_DVCC } else { TYPE_DVVC },
        &mut body,
    );
    emit(ID_BLOCK_ADD_ID_EXTRA_DATA, record, &mut body);
    let mut out = Vec::new();
    emit(ID_BLOCK_ADDITION_MAPPING, &body, &mut out);
    out
}

fn emit(id: u32, body: &[u8], out: &mut Vec<u8>) {
    ebml::write_id(id, out);
    ebml::write_size_at(body.len() as u64, ebml::min_size_width(body.len() as u64), out)
        .expect("a body this size always fits");
    out.extend_from_slice(body);
}

/// Whether a BlockAdditionMapping is Dolby Vision signalling.
pub fn is_dv_mapping(buf: &[u8], el: &Elem) -> bool {
    if el.id != ID_BLOCK_ADDITION_MAPPING {
        return false;
    }
    let Ok(kids) = ebml::children(buf, el.body, el.body_end()) else {
        return false;
    };
    kids.iter().any(|k| {
        k.id == ID_BLOCK_ADD_ID_TYPE
            && k.size == 4
            && matches!(&buf[k.body..k.body + 4], b if b == TYPE_DVVC || b == TYPE_DVCC)
    })
}

/// Read the record out of a TrackEntry, if it carries one.
pub fn find_in_track_entry(buf: &[u8], entry: &Elem) -> Option<DoviConfig> {
    let kids = ebml::children(buf, entry.body, entry.body_end()).ok()?;
    for k in kids.iter().filter(|k| is_dv_mapping(buf, k)) {
        let inner = ebml::children(buf, k.body, k.body_end()).ok()?;
        if let Some(extra) = ebml::find(&inner, ID_BLOCK_ADD_ID_EXTRA_DATA) {
            return DoviConfig::decode(&buf[extra.body..extra.body_end()]);
        }
    }
    None
}

/// What to do with the mapping when a TrackEntry is rebuilt.
pub enum DvMapping {
    Keep,
    Remove,
    Set(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compatibility_id_follows_the_rpus_and_the_picture() {
        // Profile 5 is none, whatever the picture says.
        assert_eq!(compat_for(5, Some(16)).id, 0);
        assert_eq!(compat_for(5, None).id, 0);
        // Everything else by the transfer: PQ, HLG, SDR.
        assert_eq!(compat_for(8, Some(16)).id, 1);
        assert_eq!(compat_for(8, Some(18)).id, 4);
        assert_eq!(compat_for(8, Some(1)).id, 2);
        assert_eq!(compat_for(8, Some(14)).id, 2);
        // Profile 7 that keeps its enhancement layer: what discs say.
        assert_eq!(compat_for(7, Some(16)).id, 6);
        // Unsaid: HDR10, the common case, and the report says it was a guess.
        let c = compat_for(8, None);
        assert_eq!(c.id, 1);
        assert!(c.why.contains("does not say") && !c.sure);
    }

    #[test]
    fn a_chosen_id_that_contradicts_the_file_is_called_out() {
        assert!(compat_doubt(5, 1, Some(16)).is_some());
        assert!(compat_doubt(8, 0, Some(16)).is_some());
        assert!(compat_doubt(8, 1, Some(18)).unwrap().contains("HLG"));
        assert!(compat_doubt(8, 4, Some(16)).unwrap().contains("PQ"));
        assert!(compat_doubt(8, 2, Some(18)).is_some());
        // Agreement, and choices about a picture that says nothing, pass.
        assert_eq!(compat_doubt(5, 0, Some(16)), None);
        assert_eq!(compat_doubt(8, 1, Some(16)), None);
        assert_eq!(compat_doubt(8, 4, Some(18)), None);
        assert_eq!(compat_doubt(8, 2, Some(1)), None);
        assert_eq!(compat_doubt(8, 4, None), None);
    }

    /// The record mkvmerge wrote for the AV1 test encode: profile 10, level 6,
    /// BL+RPU, HDR10 compatible.
    const REFERENCE: [u8; 24] = [
        0x01, 0x00, 0x14, 0x35, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn the_reference_record_decodes_to_what_mediainfo_reports() {
        let c = DoviConfig::decode(&REFERENCE).unwrap();
        assert_eq!(c.profile, 10);
        assert_eq!(c.level, 6);
        assert!(c.rpu_present && c.bl_present && !c.el_present);
        assert_eq!(c.bl_signal_compatibility_id, 1);
        assert_eq!(c.encode(), REFERENCE, "and encodes back byte for byte");
    }

    #[test]
    fn levels_follow_the_pixel_rate() {
        assert_eq!(level_for(3840, 2160, 24000.0 / 1001.0), 6);
        assert_eq!(level_for(1920, 1080, 24000.0 / 1001.0), 3);
        assert_eq!(level_for(1920, 1080, 25.0), 4);
        assert_eq!(level_for(3840, 2160, 60.0), 9);
        assert_eq!(level_for(3840, 2160, 120.0), 10);
        assert_eq!(level_for(7680, 4320, 30.0), 11);
    }

    #[test]
    fn the_mapping_element_reads_back_through_the_parser() {
        let bytes = mapping_element(&REFERENCE, false);
        let el = ebml::read_elem(&bytes, 0).unwrap();
        assert!(is_dv_mapping(&bytes, &el));
        // Wrap it in a fake TrackEntry to exercise the finder.
        let mut entry = Vec::new();
        emit(ebml::ID_TRACK_ENTRY, &bytes, &mut entry);
        let entry_el = ebml::read_elem(&entry, 0).unwrap();
        let found = find_in_track_entry(&entry, &entry_el).unwrap();
        assert_eq!(found, DoviConfig::decode(&REFERENCE).unwrap());
    }

    #[test]
    fn a_mapping_of_another_kind_is_left_alone() {
        let mut body = Vec::new();
        emit(ID_BLOCK_ADD_ID_TYPE, b"mvcC", &mut body);
        let mut out = Vec::new();
        emit(ID_BLOCK_ADDITION_MAPPING, &body, &mut out);
        let el = ebml::read_elem(&out, 0).unwrap();
        assert!(!is_dv_mapping(&out, &el));
    }
}
