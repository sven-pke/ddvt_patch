//! The Colour element: static HDR signalling, and how to rewrite it.
//!
//! Every value here is a fixed-size field, which is what makes an in-place patch
//! possible. When a new value happens to need a wider field than the old one,
//! the element grows by a byte or two and the caller reclaims that from a Void.

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::ebml::{self, Elem};

/// Mastering display chromaticities and luminance, in the units a human uses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Mdcv {
    /// Red, green, blue chromaticity pairs.
    pub primaries: [(f64, f64); 3],
    pub white_point: (f64, f64),
    /// cd/m2.
    pub luminance_max: f64,
    /// cd/m2.
    pub luminance_min: f64,
}

impl Mdcv {
    pub fn preset(name: &str) -> Result<[(f64, f64); 3]> {
        Ok(match name.to_ascii_lowercase().replace(['-', '_', ' '], "") {
            ref s if s == "bt2020" || s == "rec2020" => {
                [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)]
            }
            ref s if s == "displayp3" || s == "p3d65" => {
                [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)]
            }
            ref s if s == "dcip3" || s == "p3dci" => {
                [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)]
            }
            ref s if s == "bt709" || s == "rec709" => {
                [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)]
            }
            other => bail!(
                "unknown mastering display preset {other:?}; \
                 known: bt2020, display-p3, dci-p3, bt709"
            ),
        })
    }

    pub fn preset_white_point(name: &str) -> (f64, f64) {
        match name.to_ascii_lowercase().replace(['-', '_', ' '], "") {
            ref s if s == "dcip3" || s == "p3dci" => (0.314, 0.351),
            _ => (0.3127, 0.3290), // D65
        }
    }
}

/// What a file currently says.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ColourValues {
    pub matrix_coefficients: Option<u64>,
    pub range: Option<u64>,
    pub transfer_characteristics: Option<u64>,
    pub primaries: Option<u64>,
    pub max_cll: Option<u64>,
    pub max_fall: Option<u64>,
    pub mdcv: Option<Mdcv>,
}

/// What the user wants changed. `None` means leave alone.
#[derive(Debug, Clone, Default)]
pub struct ColourPatch {
    pub max_cll: Option<u64>,
    pub max_fall: Option<u64>,
    pub primaries_xy: Option<[(f64, f64); 3]>,
    pub white_point: Option<(f64, f64)>,
    pub luminance_max: Option<f64>,
    pub luminance_min: Option<f64>,
    pub matrix_coefficients: Option<u64>,
    pub range: Option<u64>,
    pub transfer_characteristics: Option<u64>,
    pub colour_primaries: Option<u64>,
    /// Element IDs to drop from Colour entirely. Used when the bitstream carries
    /// the same value: two copies of a number are two chances to disagree, and
    /// MediaInfo reports the duplicate as a separate "_Original" field.
    pub remove: Vec<u32>,
}

impl ColourPatch {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty()
            && self.max_cll.is_none()
            && self.max_fall.is_none()
            && self.primaries_xy.is_none()
            && self.white_point.is_none()
            && self.luminance_max.is_none()
            && self.luminance_min.is_none()
            && self.matrix_coefficients.is_none()
            && self.range.is_none()
            && self.transfer_characteristics.is_none()
            && self.colour_primaries.is_none()
    }

    fn touches_mastering_display(&self) -> bool {
        !self.remove.contains(&ebml::ID_MASTERING_METADATA)
            && (self.primaries_xy.is_some()
            || self.white_point.is_some()
            || self.luminance_max.is_some()
            || self.luminance_min.is_some())
    }
}

const MDCV_FIELDS: [u32; 10] = [
    ebml::ID_PRIMARY_R_X,
    ebml::ID_PRIMARY_R_Y,
    ebml::ID_PRIMARY_G_X,
    ebml::ID_PRIMARY_G_Y,
    ebml::ID_PRIMARY_B_X,
    ebml::ID_PRIMARY_B_Y,
    ebml::ID_WHITE_X,
    ebml::ID_WHITE_Y,
    ebml::ID_LUMINANCE_MAX,
    ebml::ID_LUMINANCE_MIN,
];

/// Read the Colour element, if the track has one.
pub fn read(buf: &[u8], colour: Option<&Elem>) -> Result<ColourValues> {
    let mut out = ColourValues::default();
    let Some(colour) = colour else {
        return Ok(out);
    };
    let kids = ebml::children(buf, colour.body, colour.body_end())?;
    let uint = |id: u32| -> Result<Option<u64>> {
        match ebml::find(&kids, id) {
            Some(e) => Ok(Some(ebml::read_uint(buf, e)?)),
            None => Ok(None),
        }
    };
    out.matrix_coefficients = uint(ebml::ID_MATRIX_COEFFICIENTS)?;
    out.range = uint(ebml::ID_RANGE)?;
    out.transfer_characteristics = uint(ebml::ID_TRANSFER_CHARACTERISTICS)?;
    out.primaries = uint(ebml::ID_PRIMARIES)?;
    out.max_cll = uint(ebml::ID_MAX_CLL)?;
    out.max_fall = uint(ebml::ID_MAX_FALL)?;

    if let Some(mm) = ebml::find(&kids, ebml::ID_MASTERING_METADATA) {
        let mk = ebml::children(buf, mm.body, mm.body_end())?;
        let f = |id: u32| -> Result<f64> {
            match ebml::find(&mk, id) {
                Some(e) => ebml::read_float(buf, e),
                None => Ok(0.0),
            }
        };
        out.mdcv = Some(Mdcv {
            primaries: [
                (f(ebml::ID_PRIMARY_R_X)?, f(ebml::ID_PRIMARY_R_Y)?),
                (f(ebml::ID_PRIMARY_G_X)?, f(ebml::ID_PRIMARY_G_Y)?),
                (f(ebml::ID_PRIMARY_B_X)?, f(ebml::ID_PRIMARY_B_Y)?),
            ],
            white_point: (f(ebml::ID_WHITE_X)?, f(ebml::ID_WHITE_Y)?),
            luminance_max: f(ebml::ID_LUMINANCE_MAX)?,
            luminance_min: f(ebml::ID_LUMINANCE_MIN)?,
        });
    }
    Ok(out)
}

/// Emit one unsigned-integer element, reusing the old field width when the new
/// value still fits so the element keeps its length.
fn emit_uint(id: u32, value: u64, old: Option<&Elem>, out: &mut Vec<u8>) -> Result<()> {
    let old_width = old.map(|e| e.size as usize).unwrap_or(0);
    let need = ebml::min_uint_width(value);
    let width = if old_width >= need { old_width } else { need };
    ebml::write_id(id, out);
    ebml::write_size_at(width as u64, old.map(|e| e.size_len).unwrap_or(1), out)?;
    ebml::write_uint_at(value, width, out)
}

fn emit_float(id: u32, value: f64, old: Option<&Elem>, out: &mut Vec<u8>) -> Result<()> {
    let width = match old.map(|e| e.size) {
        Some(8) => 8,
        _ => 4,
    };
    ebml::write_id(id, out);
    ebml::write_size_at(width as u64, old.map(|e| e.size_len).unwrap_or(1), out)?;
    ebml::write_float_at(value, width, out)
}

/// Wrap a body in an element header, keeping a given size width when possible.
fn wrap(id: u32, body: &[u8], preferred_size_len: usize, out: &mut Vec<u8>) -> Result<()> {
    let len = body.len() as u64;
    let need = ebml::min_size_width(len);
    let width = preferred_size_len.max(need);
    ebml::write_id(id, out);
    ebml::write_size_at(len, width, out)?;
    out.extend_from_slice(body);
    Ok(())
}

/// Build the replacement MasteringMetadata body.
fn build_mastering(
    buf: &[u8],
    old: Option<&Elem>,
    patch: &ColourPatch,
    current: Option<&Mdcv>,
) -> Result<Vec<u8>> {
    let old_kids = match old {
        Some(e) => ebml::children(buf, e.body, e.body_end())?,
        None => Vec::new(),
    };

    // Start from what the file says, so a partial patch keeps the rest.
    let base = current.copied().unwrap_or(Mdcv {
        primaries: [(0.0, 0.0); 3],
        white_point: (0.0, 0.0),
        luminance_max: 0.0,
        luminance_min: 0.0,
    });
    let primaries = patch.primaries_xy.unwrap_or(base.primaries);
    let white_point = patch.white_point.unwrap_or(base.white_point);
    let luminance_max = patch.luminance_max.unwrap_or(base.luminance_max);
    let luminance_min = patch.luminance_min.unwrap_or(base.luminance_min);

    let values = [
        primaries[0].0,
        primaries[0].1,
        primaries[1].0,
        primaries[1].1,
        primaries[2].0,
        primaries[2].1,
        white_point.0,
        white_point.1,
        luminance_max,
        luminance_min,
    ];

    // Keep the file's own element order; append anything it was missing.
    let mut body = Vec::new();
    let mut written = [false; 10];
    for kid in &old_kids {
        match MDCV_FIELDS.iter().position(|id| *id == kid.id) {
            Some(i) => {
                emit_float(MDCV_FIELDS[i], values[i], Some(kid), &mut body)?;
                written[i] = true;
            }
            None => body.extend_from_slice(&buf[kid.offset..kid.body_end()]),
        }
    }
    for (i, id) in MDCV_FIELDS.iter().enumerate() {
        if !written[i] {
            emit_float(*id, values[i], None, &mut body)?;
        }
    }
    Ok(body)
}

/// Build the replacement Colour element, header included.
pub fn build(buf: &[u8], colour: Option<&Elem>, patch: &ColourPatch) -> Result<Vec<u8>> {
    let current = read(buf, colour)?;
    let old_kids = match colour {
        Some(e) => ebml::children(buf, e.body, e.body_end())?,
        None => Vec::new(),
    };
    if old_kids.iter().any(|e| e.id == ebml::ID_CRC32) {
        bail!("the Colour element carries a CRC-32; this patcher does not recompute it");
    }

    let drop_mastering = patch.remove.contains(&ebml::ID_MASTERING_METADATA);
    let want_mastering =
        !drop_mastering && (patch.touches_mastering_display() || current.mdcv.is_some());
    let old_mastering = ebml::find(&old_kids, ebml::ID_MASTERING_METADATA).copied();

    let simple: [(u32, Option<u64>); 6] = [
        (ebml::ID_MATRIX_COEFFICIENTS, patch.matrix_coefficients),
        (ebml::ID_RANGE, patch.range),
        (
            ebml::ID_TRANSFER_CHARACTERISTICS,
            patch.transfer_characteristics,
        ),
        (ebml::ID_PRIMARIES, patch.colour_primaries),
        (ebml::ID_MAX_CLL, patch.max_cll),
        (ebml::ID_MAX_FALL, patch.max_fall),
    ];

    let mut body = Vec::new();
    let mut written = [false; 6];
    let mut mastering_written = false;

    for kid in &old_kids {
        if kid.id == ebml::ID_MASTERING_METADATA {
            if want_mastering {
                let inner = build_mastering(buf, Some(kid), patch, current.mdcv.as_ref())?;
                wrap(ebml::ID_MASTERING_METADATA, &inner, kid.size_len, &mut body)?;
            }
            mastering_written = true;
            continue;
        }
        if patch.remove.contains(&kid.id) {
            if let Some(i) = simple.iter().position(|(id, _)| *id == kid.id) {
                written[i] = true; // dropped on purpose, do not re-add below
            }
            continue;
        }
        match simple.iter().position(|(id, _)| *id == kid.id) {
            Some(i) => {
                let value = simple[i].1;
                match value {
                    Some(v) => emit_uint(kid.id, v, Some(kid), &mut body)?,
                    None => body.extend_from_slice(&buf[kid.offset..kid.body_end()]),
                }
                written[i] = true;
            }
            None => body.extend_from_slice(&buf[kid.offset..kid.body_end()]),
        }
    }

    // Fields the file never had but the user asked for.
    for (i, (id, value)) in simple.iter().enumerate() {
        if !written[i] && !patch.remove.contains(id) {
            if let Some(v) = value {
                emit_uint(*id, *v, None, &mut body)?;
            }
        }
    }
    if want_mastering && !mastering_written {
        let inner = build_mastering(buf, old_mastering.as_ref(), patch, current.mdcv.as_ref())?;
        wrap(ebml::ID_MASTERING_METADATA, &inner, 1, &mut body)?;
    }

    let mut out = Vec::new();
    wrap(
        ebml::ID_COLOUR,
        &body,
        colour.map(|e| e.size_len).unwrap_or(1),
        &mut out,
    )
    .context("building the replacement Colour element")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Colour element the way mkvmerge does, for the tests to chew on.
    fn sample_colour() -> Vec<u8> {
        let mut body = Vec::new();
        emit_uint(ebml::ID_MATRIX_COEFFICIENTS, 9, None, &mut body).unwrap();
        emit_uint(ebml::ID_TRANSFER_CHARACTERISTICS, 16, None, &mut body).unwrap();
        emit_uint(ebml::ID_PRIMARIES, 9, None, &mut body).unwrap();
        emit_uint(ebml::ID_MAX_CLL, 0, None, &mut body).unwrap();
        emit_uint(ebml::ID_MAX_FALL, 0, None, &mut body).unwrap();
        let mut mm = Vec::new();
        for (i, id) in MDCV_FIELDS.iter().enumerate() {
            let v = [0.708, 0.292, 0.170, 0.797, 0.131, 0.046, 0.3127, 0.3290, 1000.0, 0.0001][i];
            emit_float(*id, v, None, &mut mm).unwrap();
        }
        wrap(ebml::ID_MASTERING_METADATA, &mm, 1, &mut body).unwrap();
        let mut out = Vec::new();
        wrap(ebml::ID_COLOUR, &body, 1, &mut out).unwrap();
        out
    }

    #[test]
    fn reads_back_what_a_muxer_wrote() {
        let buf = sample_colour();
        let el = ebml::read_elem(&buf, 0).unwrap();
        let v = read(&buf, Some(&el)).unwrap();
        assert_eq!(v.max_cll, Some(0));
        assert_eq!(v.max_fall, Some(0));
        assert_eq!(v.transfer_characteristics, Some(16));
        let m = v.mdcv.unwrap();
        assert!((m.luminance_max - 1000.0).abs() < 1e-6);
        assert!((m.luminance_min - 0.0001).abs() < 1e-9);
        assert!((m.primaries[0].0 - 0.708).abs() < 1e-6);
    }

    #[test]
    fn zero_to_four_digits_costs_exactly_one_byte_per_field() {
        // The case from the real file: MaxCLL/MaxFALL sit at 0 in one byte each,
        // and 1768/505 need two. That is the +2 a Void has to absorb.
        let buf = sample_colour();
        let el = ebml::read_elem(&buf, 0).unwrap();
        let patch = ColourPatch {
            max_cll: Some(1768),
            max_fall: Some(505),
            ..Default::default()
        };
        let new = build(&buf, Some(&el), &patch).unwrap();
        assert_eq!(new.len(), buf.len() + 2);

        let nel = ebml::read_elem(&new, 0).unwrap();
        let v = read(&new, Some(&nel)).unwrap();
        assert_eq!(v.max_cll, Some(1768));
        assert_eq!(v.max_fall, Some(505));
        // Everything untouched survives.
        assert_eq!(v.transfer_characteristics, Some(16));
        assert!((v.mdcv.unwrap().luminance_max - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn changing_only_a_float_leaves_the_length_alone() {
        let buf = sample_colour();
        let el = ebml::read_elem(&buf, 0).unwrap();
        let patch = ColourPatch {
            luminance_min: Some(0.005),
            ..Default::default()
        };
        let new = build(&buf, Some(&el), &patch).unwrap();
        assert_eq!(new.len(), buf.len(), "float fields are fixed width");
        let nel = ebml::read_elem(&new, 0).unwrap();
        let m = read(&new, Some(&nel)).unwrap().mdcv.unwrap();
        assert!((m.luminance_min - 0.005).abs() < 1e-9);
        assert!((m.luminance_max - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn a_second_patch_to_the_same_width_is_free() {
        // Once a field is two bytes wide, another four-digit value reuses it.
        let buf = sample_colour();
        let el = ebml::read_elem(&buf, 0).unwrap();
        let once = build(
            &buf,
            Some(&el),
            &ColourPatch {
                max_cll: Some(1768),
                ..Default::default()
            },
        )
        .unwrap();
        let el2 = ebml::read_elem(&once, 0).unwrap();
        let twice = build(
            &once,
            Some(&el2),
            &ColourPatch {
                max_cll: Some(4000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(twice.len(), once.len());
        let el3 = ebml::read_elem(&twice, 0).unwrap();
        assert_eq!(read(&twice, Some(&el3)).unwrap().max_cll, Some(4000));
    }

    #[test]
    fn a_missing_mastering_display_is_created_from_a_preset() {
        let mut body = Vec::new();
        emit_uint(ebml::ID_MATRIX_COEFFICIENTS, 9, None, &mut body).unwrap();
        let mut buf = Vec::new();
        wrap(ebml::ID_COLOUR, &body, 1, &mut buf).unwrap();
        let el = ebml::read_elem(&buf, 0).unwrap();

        let patch = ColourPatch {
            primaries_xy: Some(Mdcv::preset("bt2020").unwrap()),
            white_point: Some(Mdcv::preset_white_point("bt2020")),
            luminance_max: Some(1000.0),
            luminance_min: Some(0.005),
            ..Default::default()
        };
        let new = build(&buf, Some(&el), &patch).unwrap();
        let nel = ebml::read_elem(&new, 0).unwrap();
        let m = read(&new, Some(&nel)).unwrap().mdcv.unwrap();
        assert!((m.primaries[1].1 - 0.797).abs() < 1e-6);
        assert!((m.white_point.0 - 0.3127).abs() < 1e-6);
        assert!((m.luminance_min - 0.005).abs() < 1e-9);
    }

    #[test]
    fn unknown_children_are_carried_through_untouched() {
        let mut body = Vec::new();
        emit_uint(ebml::ID_MAX_CLL, 0, None, &mut body).unwrap();
        ebml::write_id(0x55B7, &mut body); // ChromaSitingHorz
        ebml::write_size_at(1, 1, &mut body).unwrap();
        body.push(2);
        let mut buf = Vec::new();
        wrap(ebml::ID_COLOUR, &body, 1, &mut buf).unwrap();
        let el = ebml::read_elem(&buf, 0).unwrap();

        let new = build(
            &buf,
            Some(&el),
            &ColourPatch {
                max_cll: Some(1000),
                ..Default::default()
            },
        )
        .unwrap();
        let nel = ebml::read_elem(&new, 0).unwrap();
        let kids = ebml::children(&new, nel.body, nel.body_end()).unwrap();
        let siting = kids.iter().find(|e| e.id == 0x55B7).expect("kept");
        assert_eq!(ebml::read_uint(&new, siting).unwrap(), 2);
    }
}
