//! Fitting metadata taken from one file to another.
//!
//! A Dolby Vision grade knows where the picture sits inside the canvas (the
//! RPU level 5 active area) and how many frames there are. When the file that
//! receives the metadata has a different canvas - the master is cropped to the
//! picture, the encode kept the bars - or a different number of frames, the
//! metadata has to be told. This is what dovi_tool's editor does from a JSON;
//! here it happens on the way from one file to the other.

use anyhow::{bail, Context, Result};

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock;

/// How far the picture moved in from each edge, in pixels. Positive when the
/// target canvas is larger than the reference on that side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shift {
    pub left: i64,
    pub right: i64,
    pub top: i64,
    pub bottom: i64,
}

impl Shift {
    /// The reference picture centred inside the target canvas. An odd
    /// difference puts the extra line at the bottom (and right), the way
    /// encoders pad - and, going the other way, takes it from there too, so
    /// a shift and its inverse cancel out.
    pub fn centred(reference: (u64, u64), target: (u64, u64)) -> Shift {
        let dx = target.0 as i64 - reference.0 as i64;
        let dy = target.1 as i64 - reference.1 as i64;
        // Integer division truncates towards zero, which is what makes the
        // two directions mirror each other.
        Shift {
            left: dx / 2,
            right: dx - dx / 2,
            top: dy / 2,
            bottom: dy - dy / 2,
        }
    }

    pub fn is_zero(&self) -> bool {
        *self == Shift::default()
    }
}

/// Level 5 offsets as the RPU stores them: left, right, top, bottom.
pub type Offsets = [u16; 4];

/// "L,R,T,B" from the command line.
pub fn parse_offsets(s: &str) -> Result<Offsets> {
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    if parts.len() != 4 {
        bail!("active area wants four numbers, left,right,top,bottom - got {s:?}");
    }
    let mut out = [0u16; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .parse::<u16>()
            .with_context(|| format!("active area offset {p:?} is not a number"))?;
        if out[i] > 8191 {
            bail!("active area offset {} does not fit the 13 bits the RPU has", out[i]);
        }
    }
    Ok(out)
}

/// What the RPU says now, zero offsets when it carries no level 5.
pub fn offsets_of(rpu: &DoviRpu) -> Offsets {
    match rpu.vdr_dm_data.as_ref().and_then(|dm| dm.get_block(5)) {
        Some(ExtMetadataBlock::Level5(l5)) => [
            l5.active_area_left_offset,
            l5.active_area_right_offset,
            l5.active_area_top_offset,
            l5.active_area_bottom_offset,
        ],
        _ => [0; 4],
    }
}

/// The smallest offset on each edge over every RPU: where the widest
/// picture of the grade sits. A film whose aspect ratio changes has larger
/// offsets in some scenes; the bars measured on the pixels correspond to
/// these smallest ones.
pub fn min_offsets(rpus: &[DoviRpu]) -> Offsets {
    let mut out = [u16::MAX; 4];
    for rpu in rpus {
        let o = offsets_of(rpu);
        for k in 0..4 {
            out[k] = out[k].min(o[k]);
        }
    }
    if rpus.is_empty() {
        [0; 4]
    } else {
        out
    }
}

/// What the edit did, for the report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AreaChange {
    pub changed: usize,
    pub unchanged: usize,
    /// Distinct (before, after) pairs, in the order first seen, at most eight.
    pub examples: Vec<(Offsets, Offsets)>,
}

fn apply(
    rpus: &mut [DoviRpu],
    target: impl Fn(u64, Offsets) -> Result<Offsets>,
) -> Result<AreaChange> {
    let mut change = AreaChange::default();
    for (i, rpu) in rpus.iter_mut().enumerate() {
        let before = offsets_of(rpu);
        let after = target(i as u64, before)?;
        if before == after {
            change.unchanged += 1;
            continue;
        }
        if rpu.vdr_dm_data.is_none() {
            bail!("RPU {i} carries no display management data, nowhere to put an active area");
        }
        rpu.set_active_area_offsets(after[0], after[1], after[2], after[3])
            .with_context(|| format!("setting the active area of RPU {i}"))?;
        change.changed += 1;
        if change.examples.len() < 8
            && !change.examples.iter().any(|(b, a)| (*b, *a) == (before, after))
        {
            change.examples.push((before, after));
        }
    }
    Ok(change)
}

/// Move every level 5 by the shift, keeping whatever offsets the grade already
/// had. A move past the edge is an error, not a clamp: it means the two
/// pictures do not fit the way we assumed.
pub fn shift_active_area(rpus: &mut [DoviRpu], shift: Shift) -> Result<AreaChange> {
    let deltas = [shift.left, shift.right, shift.top, shift.bottom];
    apply(rpus, |i, before| {
        let mut after = [0u16; 4];
        for k in 0..4 {
            let v = before[k] as i64 + deltas[k];
            if v < 0 {
                bail!(
                    "RPU {i}: active area offset {} would become {v}; the reference \
                     picture does not fit inside the target this way",
                    ["left", "right", "top", "bottom"][k]
                );
            }
            if v > 8191 {
                bail!("RPU {i}: active area offset {v} does not fit the 13 bits the RPU has");
            }
            after[k] = v as u16;
        }
        Ok(after)
    })
}

/// Put the same level 5 into every RPU.
pub fn set_active_area(rpus: &mut [DoviRpu], offsets: Offsets) -> Result<AreaChange> {
    apply(rpus, |_, _| Ok(offsets))
}

/// Line the list up with a target whose frame 0 is the reference frame
/// `offset`. Positive drops leading entries; negative repeats the first entry,
/// for a target that starts before the reference does. Entries past the end
/// of the target are left in place - the writer ignores what it does not need.
/// `copy` makes the repeats, for types that cannot simply be cloned.
pub fn offset_frames<T>(mut list: Vec<T>, offset: i64, copy: impl Fn(&T) -> T) -> Result<Vec<T>> {
    if offset == 0 {
        return Ok(list);
    }
    if offset > 0 {
        let n = offset as usize;
        if n >= list.len() {
            bail!(
                "a frame offset of {offset} leaves nothing of the {} entries",
                list.len()
            );
        }
        list.drain(..n);
        return Ok(list);
    }
    let n = offset.unsigned_abs() as usize;
    let first = list
        .first()
        .context("cannot repeat the first entry of an empty list")?;
    let mut out = Vec::with_capacity(list.len() + n);
    for _ in 0..n {
        out.push(copy(first));
    }
    out.extend(list);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dolby_vision::rpu::generate::GenerateConfig;

    fn rpu_with_l5(offsets: Offsets) -> DoviRpu {
        let mut rpu = DoviRpu::profile81_config(&GenerateConfig::default()).unwrap();
        rpu.set_active_area_offsets(offsets[0], offsets[1], offsets[2], offsets[3])
            .unwrap();
        rpu
    }

    #[test]
    fn the_letterboxed_master_is_centred_in_the_full_canvas() {
        let s = Shift::centred((3840, 1604), (3840, 2160));
        assert_eq!(
            s,
            Shift {
                left: 0,
                right: 0,
                top: 278,
                bottom: 278
            }
        );
        assert!(!s.is_zero());
        assert!(Shift::centred((3840, 2160), (3840, 2160)).is_zero());
    }

    #[test]
    fn an_odd_difference_puts_the_extra_line_at_the_bottom() {
        let s = Shift::centred((1920, 799), (1920, 1080));
        assert_eq!((s.top, s.bottom), (140, 141));
    }

    #[test]
    fn a_larger_reference_shifts_the_other_way() {
        let s = Shift::centred((3840, 2160), (3840, 1604));
        assert_eq!((s.top, s.bottom), (-278, -278));
        // The odd line comes off the bottom, mirroring where it was added.
        let s = Shift::centred((1920, 1080), (1920, 799));
        assert_eq!((s.top, s.bottom), (-140, -141));
        let back = Shift::centred((1920, 799), (1920, 1080));
        assert_eq!((s.top + back.top, s.bottom + back.bottom), (0, 0));
    }

    #[test]
    fn shifting_keeps_what_the_grade_already_had() {
        let mut rpus = vec![rpu_with_l5([0, 0, 0, 0]), rpu_with_l5([10, 10, 20, 20])];
        let change =
            shift_active_area(&mut rpus, Shift::centred((3840, 1604), (3840, 2160))).unwrap();
        assert_eq!(change.changed, 2);
        assert_eq!(offsets_of(&rpus[0]), [0, 0, 278, 278]);
        assert_eq!(offsets_of(&rpus[1]), [10, 10, 298, 298]);
        assert_eq!(change.examples.len(), 2);
    }

    #[test]
    fn a_shift_past_the_edge_is_refused() {
        let mut rpus = vec![rpu_with_l5([0, 0, 100, 100])];
        let err = shift_active_area(&mut rpus, Shift::centred((3840, 2160), (3840, 1604)))
            .unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
    }

    #[test]
    fn a_zero_shift_changes_nothing_and_says_so() {
        let mut rpus = vec![rpu_with_l5([0, 0, 278, 278])];
        let change = shift_active_area(&mut rpus, Shift::default()).unwrap();
        assert_eq!(
            change,
            AreaChange {
                changed: 0,
                unchanged: 1,
                examples: vec![]
            }
        );
    }

    #[test]
    fn setting_replaces_whatever_was_there() {
        let mut rpus = vec![rpu_with_l5([1, 2, 3, 4]), rpu_with_l5([0, 0, 0, 0])];
        let change = set_active_area(&mut rpus, [0, 0, 278, 278]).unwrap();
        assert_eq!(change.changed, 2);
        assert!(rpus.iter().all(|r| offsets_of(r) == [0, 0, 278, 278]));
    }

    #[test]
    fn the_edit_survives_a_round_trip_through_the_bytes() {
        let mut rpus = vec![rpu_with_l5([0, 0, 0, 0])];
        shift_active_area(&mut rpus, Shift::centred((3840, 1604), (3840, 2160))).unwrap();
        let bytes = rpus[0].write_hevc_unspec62_nalu().unwrap();
        let back = DoviRpu::parse_unspec62_nalu(&bytes).unwrap();
        assert_eq!(offsets_of(&back), [0, 0, 278, 278]);
    }

    #[test]
    fn offsets_parse_and_reject_junk() {
        assert_eq!(parse_offsets("0,0,278,278").unwrap(), [0, 0, 278, 278]);
        assert_eq!(parse_offsets(" 1, 2 ,3,4 ").unwrap(), [1, 2, 3, 4]);
        assert!(parse_offsets("1,2,3").is_err());
        assert!(parse_offsets("a,b,c,d").is_err());
        assert!(parse_offsets("0,0,9000,0").is_err());
    }

    #[test]
    fn frame_offsets_drop_or_repeat() {
        let list = vec![1, 2, 3, 4];
        let c = |v: &i32| *v;
        assert_eq!(offset_frames(list.clone(), 0, c).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(offset_frames(list.clone(), 2, c).unwrap(), vec![3, 4]);
        assert_eq!(offset_frames(list.clone(), -2, c).unwrap(), vec![1, 1, 1, 2, 3, 4]);
        assert!(offset_frames(list.clone(), 4, c).is_err());
        assert!(offset_frames(Vec::<i32>::new(), -1, c).is_err());
    }
}
