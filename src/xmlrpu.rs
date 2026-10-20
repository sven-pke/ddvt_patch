//! A Dolby Vision metadata XML as the source of the RPUs.
//!
//! Grading houses hand out the CM metadata as XML, not as an RPU binary: a
//! list of shots with their trims, and the canvas the grade was made for. The
//! old injector took such a file by dragging it onto the window and turned it
//! into an RPU with `dovi_tool generate --xml`, passing the video's width and
//! height as the canvas. This does the same thing in process, with the same
//! library dovi_tool uses, so no second tool has to be there.
//!
//! The canvas matters: the active area (level 5) in the XML is given as an
//! aspect ratio, and it only becomes pixel offsets once the canvas is known.
//! Pass the picture the RPUs are going to sit on, and the letterbox bars come
//! out right.
//!
//! So does the picture's transfer. The RPUs map the base layer to the
//! Dolby Vision signal: from a PQ picture that is the identity (8.1), from
//! an HLG picture it is a curve of its own (8.4). The XML does not say which;
//! the library makes 8.1 unless told, as dovi_tool does without `-p`. For an
//! SDR picture there is no mapping to make, and the XML is refused.

use anyhow::{bail, Context, Result};
use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::generate::GenerateProfile;
use dolby_vision::rpu::vdr_dm_data::CmVersion;
use dolby_vision::xml::{CmXmlParser, XmlParserOpts};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

/// True when the path names a metadata XML rather than an RPU binary.
pub fn is_xml(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
}

/// What came out of an XML, and what it was made from.
pub struct Generated {
    pub rpus: Vec<DoviRpu>,
    /// The canvas the RPUs were generated for.
    pub canvas: (u64, u64),
    /// How many shots the XML describes.
    pub shots: usize,
    pub cm_version: CmVersion,
    /// The profile the RPUs carry, in words.
    pub profile: String,
    /// The frame rate the grade was made at, as the XML's EditRate has it.
    /// The library ignores it, and it is the one thing that says whether the
    /// shots in the XML mean the same frames as the film does.
    pub rate: Option<(u32, u32)>,
}

impl Generated {
    pub fn words(&self) -> String {
        format!(
            "{} units from {} shots, {} on a {}x{} canvas, {}",
            self.rpus.len(),
            self.shots,
            match self.cm_version {
                CmVersion::V29 => "CM v2.9",
                CmVersion::V40 => "CM v4.0",
            },
            self.canvas.0,
            self.canvas.1,
            self.profile
        )
    }
}

/// The frame rate the XML was graded at: `<EditRate>24000 1001</EditRate>`,
/// the two numbers separated by a space or a comma.
pub fn edit_rate(text: &str) -> Option<(u32, u32)> {
    let body = text.split("<EditRate>").nth(1)?.split("</EditRate>").next()?;
    let mut parts = body
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|p| !p.is_empty())
        .map(|p| p.trim().parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(num)), Some(Ok(den))) if num > 0 && den > 0 => Some((num, den)),
        // A single number is a whole-frame rate: 25, 30.
        (Some(Ok(num)), None) if num > 0 => Some((num, 1)),
        _ => None,
    }
}

/// Generate one RPU per frame from a metadata XML, for this canvas and a
/// picture with this transfer (H.273; None when it states none).
pub fn generate(path: &Path, canvas: (u64, u64), transfer: Option<u8>) -> Result<Generated> {
    let profile = match transfer {
        Some(18) => GenerateProfile::Profile84,
        t if crate::dovi_config::compat_for(8, t).id == 2 => bail!(
            "this picture is SDR: a Dolby Vision XML becomes RPUs for a PQ picture (8.1, 10.1) \
             or an HLG one (8.4, 10.4), and nothing makes the mapping an SDR picture would need"
        ),
        _ => GenerateProfile::Profile81,
    };
    let (w, h) = canvas;
    let opts = XmlParserOpts {
        canvas_width: Some(u16::try_from(w).context("the picture is wider than a canvas can be")?),
        canvas_height: Some(u16::try_from(h).context("the picture is taller than a canvas can be")?),
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the metadata XML {}", path.display()))?;
    let rate = edit_rate(&text);
    // The library parses the document with an unwrap: anything that is not
    // well-formed XML takes the whole program down with a panic instead of a
    // sentence. Catch it here and say what is wrong with the file. The hook
    // is silenced meanwhile so the crash text does not reach the log.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let parsed = catch_unwind(AssertUnwindSafe(|| CmXmlParser::new(text, opts)));
    std::panic::set_hook(hook);
    let parser = match parsed {
        Ok(r) => r.with_context(|| format!("reading the metadata XML {}", path.display()))?,
        Err(_) => bail!(
            "{} is not a Dolby Vision metadata XML that can be read: the document itself is \
             broken (truncated, or not XML at all)",
            path.display()
        ),
    };
    let mut config = parser.config;
    config.profile = profile;
    let shots = config.shots.len();
    let cm_version = config.cm_version;
    let profile = config.profile.to_string();
    let rpus = config
        .generate_rpu_list()
        .with_context(|| format!("making RPUs out of {}", path.display()))?;
    if rpus.is_empty() {
        bail!(
            "{} describes no frames; there is nothing to put in the file",
            path.display()
        );
    }
    Ok(Generated {
        rpus,
        canvas,
        shots,
        cm_version,
        profile,
        rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn only_an_xml_ending_counts_as_one() {
        assert!(is_xml(Path::new("meta.xml")));
        assert!(is_xml(Path::new("META.XML")));
        assert!(!is_xml(Path::new("rpu.bin")));
        assert!(!is_xml(Path::new("xml")));
    }

    #[test]
    fn the_edit_rate_says_what_the_grade_was_made_at() {
        assert_eq!(
            edit_rate("<x><EditRate>24000 1001</EditRate></x>"),
            Some((24000, 1001))
        );
        assert_eq!(edit_rate("<EditRate>25,1</EditRate>"), Some((25, 1)));
        assert_eq!(edit_rate("<EditRate>\n  30  1\n</EditRate>"), Some((30, 1)));
        assert_eq!(edit_rate("<EditRate>50</EditRate>"), Some((50, 1)));
        assert_eq!(edit_rate("<EditRate></EditRate>"), None);
        assert_eq!(edit_rate("no rate here"), None);
    }

    /// The library parses the document with an unwrap. A half-written file
    /// must come back as a sentence, never as a crash.
    #[test]
    fn a_broken_xml_is_refused_not_a_crash() {
        let dir = std::env::temp_dir().join("ddvt_xml_traps");
        let _ = std::fs::create_dir_all(&dir);
        let half = dir.join("half.xml");
        std::fs::write(&half, b"<?xml version=\"1.0\"?>\n<DolbyLabsMDF><Version>4.0.2").unwrap();
        let why = generate(&half, (3840, 2160), None).err().expect("a broken XML must be refused");
        assert!(format!("{why}").contains("broken"), "{why}");

        let empty = dir.join("empty.xml");
        std::fs::write(&empty, b"").unwrap();
        assert!(generate(&empty, (3840, 2160), None).is_err());
        let _ = std::fs::remove_file(&half);
        let _ = std::fs::remove_file(&empty);
    }

    /// dovi_tool's own test XMLs, next door in the workspace: a real CM v4.0
    /// file has to come out as one RPU per frame, with the letterbox the
    /// canvas implies.
    fn asset(name: &str) -> Option<PathBuf> {
        let p = Path::new("../dovi_tool/assets/tests").join(name);
        p.is_file().then_some(p)
    }

    #[test]
    fn a_metadata_xml_becomes_one_rpu_per_frame() {
        let Some(xml) = asset("cmv4_0_2.xml") else { return };
        let g = generate(&xml, (3840, 2160), Some(16)).unwrap();
        assert_eq!(g.rpus.len(), 259);
        assert_eq!(g.shots, 3);
        assert_eq!(g.cm_version, CmVersion::V40);
        assert!(g.profile.contains("8.1"), "{}", g.profile);
        // Every unit must parse back out of its own bytes.
        let bytes = g.rpus[0].write_hevc_unspec62_nalu().unwrap();
        assert!(bytes.len() > 16);
    }

    #[test]
    fn the_canvas_decides_where_the_black_bars_sit() {
        let Some(xml) = asset("cmv4_0_2.xml") else { return };
        let bars = |canvas| {
            let g = generate(&xml, canvas, None).unwrap();
            let dm = g.rpus[0].vdr_dm_data.as_ref().unwrap();
            match dm.get_block(5) {
                Some(dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock::Level5(l5)) => {
                    (l5.active_area_left_offset, l5.active_area_right_offset,
                     l5.active_area_top_offset, l5.active_area_bottom_offset)
                }
                _ => (0, 0, 0, 0),
            }
        };
        // This XML pillarboxes a 1.56 image on a 1.78 canvas: the bars are
        // a share of the width, so half the canvas gives half the bars.
        assert_eq!(bars((3840, 2160)), (240, 240, 0, 0));
        assert_eq!(bars((1920, 1080)), (120, 120, 0, 0));
    }

    #[test]
    fn the_picture_decides_the_mapping() {
        let Some(xml) = asset("cmv4_0_2.xml") else { return };
        let pq = generate(&xml, (3840, 2160), Some(16)).unwrap();
        let unsaid = generate(&xml, (3840, 2160), None).unwrap();
        let hlg = generate(&xml, (3840, 2160), Some(18)).unwrap();
        assert!(pq.profile.contains("8.1") && unsaid.profile.contains("8.1"));
        assert!(hlg.profile.contains("8.4"), "{}", hlg.profile);
        let bytes = |g: &Generated| g.rpus[0].write_hevc_unspec62_nalu().unwrap();
        assert_eq!(bytes(&pq), bytes(&unsaid));
        assert_ne!(bytes(&pq), bytes(&hlg), "HLG needs a mapping of its own");
        // SDR: refused, with the reason.
        for t in [1, 14] {
            let why = generate(&xml, (3840, 2160), Some(t)).err().expect("SDR is refused");
            assert!(format!("{why}").contains("SDR"), "{why}");
        }
    }
}
