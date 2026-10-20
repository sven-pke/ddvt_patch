//! Engine A: patch the head in place.
//!
//! The whole trick is that the head must keep its exact length, so that not a
//! single Cluster byte moves. Anything the Colour element gains is taken from a
//! Void, and the SeekHead positions of whatever shifted are corrected. What
//! reaches the disk is one write of a few kilobytes, whatever the file weighs.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

use crate::colour::{self, ColourPatch};
use crate::dovi_config::{self, DvMapping};
use crate::ebml::{self, Elem};
use crate::mkv::Head;

const ID_DEFAULT_DURATION: u32 = 0x23E383;

pub struct HeadPatch {
    /// The rebuilt head, exactly as long as the original.
    pub new_head: Vec<u8>,
    /// Bytes the Colour element gained (negative if it shrank).
    pub delta: i64,
    pub void_offset: Option<usize>,
    pub void_before: u64,
    pub void_after: u64,
    /// The narrowest byte range that actually differs - all we need to write.
    pub write_offset: usize,
    pub write_len: usize,
    pub moved: Vec<(u32, u64, u64)>,
}

impl HeadPatch {
    pub fn is_noop(&self) -> bool {
        self.write_len == 0
    }
}

fn ensure_no_crc(buf: &[u8], el: &Elem, what: &str) -> Result<()> {
    let kids = ebml::children(buf, el.body, el.body_end())?;
    if kids.iter().any(|e| e.id == ebml::ID_CRC32) {
        bail!("{what} carries a CRC-32; this patcher does not recompute checksums");
    }
    Ok(())
}

/// Re-wrap a master element around a new body. In-place patching insists the
/// header keeps its original width so the parent accounting stays valid; a
/// rewrite is free to widen it.
fn rewrap(el: &Elem, body: &[u8], what: &str, keep_width: bool, out: &mut Vec<u8>) -> Result<()> {
    let need = ebml::min_size_width(body.len() as u64);
    let width = if keep_width { el.size_len } else { el.size_len.max(need) };
    ebml::write_id(el.id, out);
    ebml::write_size_at(body.len() as u64, width, out).with_context(|| {
        format!(
            "{what} would need a wider size field ({need} bytes) than the file reserved ({})",
            el.size_len
        )
    })?;
    out.extend_from_slice(body);
    Ok(())
}

/// The Tracks element with the video track Colour and, optionally, its
/// CodecPrivate replaced. Everything else is copied byte for byte.
///
/// A CRC-32 inside any of the three rebuilt elements would be stale
/// afterwards. In place (`keep_widths`) that is a refusal; a rewrite simply
/// leaves the checksum out, which is what the ffmpeg-muxed files that carry
/// one lose and nothing else.
pub fn rebuild_tracks(
    head: &Head,
    new_colour: &[u8],
    new_codec_private: Option<&[u8]>,
    keep_widths: bool,
    dv: DvMapping,
    default_duration: Option<u64>,
) -> Result<Vec<u8>> {
    let buf = &head.buf;
    let v = &head.video;
    let tracks = ebml::find(&head.top, ebml::ID_TRACKS).context("no Tracks element")?;
    if keep_widths {
        ensure_no_crc(buf, tracks, "Tracks")?;
        ensure_no_crc(buf, &v.track_entry, "the video TrackEntry")?;
        ensure_no_crc(buf, &v.video, "the Video element")?;
    }
    let skip_crc = |e: &Elem| !keep_widths && e.id == ebml::ID_CRC32;

    // Video body: the children in order, Colour swapped for the new one, or
    // appended when there was none.
    let mut video_body = Vec::new();
    let mut colour_written = false;
    for kid in ebml::children(buf, v.video.body, v.video.body_end())? {
        if skip_crc(&kid) {
            continue;
        }
        if Some(kid.offset) == v.colour.map(|c| c.offset) {
            video_body.extend_from_slice(new_colour);
            colour_written = true;
        } else {
            video_body.extend_from_slice(&buf[kid.offset..kid.body_end()]);
        }
    }
    // A track that never had a Colour element gets one only when there is
    // something to put in it; an empty one would move every byte behind it
    // for nothing.
    let colour_has_body = ebml::read_elem(new_colour, 0).map(|e| e.size > 0).unwrap_or(false);
    if !colour_written && colour_has_body {
        video_body.extend_from_slice(new_colour);
    }
    let mut video_el = Vec::new();
    rewrap(&v.video, &video_body, "the Video element", keep_widths, &mut video_el)?;

    // TrackEntry body: the children in their original order, with Video and
    // CodecPrivate swapped for their replacements where they stand.
    let mut entry_body = Vec::new();
    let mut dd_written = false;
    for kid in ebml::children(buf, v.track_entry.body, v.track_entry.body_end())? {
        if skip_crc(&kid) {
            continue;
        }
        if kid.offset == v.video.offset {
            entry_body.extend_from_slice(&video_el);
            // Dolby Vision signalling goes right behind Video, where mkvmerge
            // puts it. Any mapping the source had is dropped further down.
            if let DvMapping::Set(mapping) = &dv {
                entry_body.extend_from_slice(mapping);
            }
        } else if !matches!(dv, DvMapping::Keep) && dovi_config::is_dv_mapping(buf, &kid) {
            continue;
        } else if kid.id == ebml::ID_CODEC_PRIVATE && new_codec_private.is_some() {
            let cp = new_codec_private.unwrap();
            ebml::write_id(ebml::ID_CODEC_PRIVATE, &mut entry_body);
            let width = if keep_widths {
                kid.size_len
            } else {
                kid.size_len.max(ebml::min_size_width(cp.len() as u64))
            };
            ebml::write_size_at(cp.len() as u64, width, &mut entry_body)
                .context("CodecPrivate would need a wider size field than the file reserved")?;
            entry_body.extend_from_slice(cp);
        } else if kid.id == ID_DEFAULT_DURATION && default_duration.is_some() {
            let value = default_duration.unwrap();
            let width = if keep_widths {
                kid.size as usize
            } else {
                (kid.size as usize).max(ebml::min_uint_width(value))
            };
            ebml::write_id(ID_DEFAULT_DURATION, &mut entry_body);
            ebml::write_size_at(width as u64, kid.size_len, &mut entry_body)?;
            ebml::write_uint_at(value, width, &mut entry_body)
                .context("DefaultDuration would need a wider field than the file reserved")?;
            dd_written = true;
        } else {
            entry_body.extend_from_slice(&buf[kid.offset..kid.body_end()]);
        }
    }
    if let (Some(value), false) = (default_duration, dd_written) {
        // The track never stated a frame duration: add one, four bytes wide
        // like every muxer writes it.
        ebml::write_id(ID_DEFAULT_DURATION, &mut entry_body);
        ebml::write_size_at(4, 1, &mut entry_body)?;
        ebml::write_uint_at(value, 4, &mut entry_body)?;
    }
    let mut entry_el = Vec::new();
    rewrap(&v.track_entry, &entry_body, "the video TrackEntry", keep_widths, &mut entry_el)?;

    let mut tracks_body = Vec::new();
    for kid in ebml::children(buf, tracks.body, tracks.body_end())? {
        if skip_crc(&kid) {
            continue;
        }
        if kid.offset == v.track_entry.offset {
            tracks_body.extend_from_slice(&entry_el);
        } else {
            tracks_body.extend_from_slice(&buf[kid.offset..kid.body_end()]);
        }
    }
    let mut tracks_el = Vec::new();
    rewrap(tracks, &tracks_body, "the Tracks element", keep_widths, &mut tracks_el)?;
    Ok(tracks_el)
}

/// Work out the new head without touching the file.
///
/// `extra_shrink` asks the head to end that many bytes earlier than it does now,
/// which is how room is made for bytes inserted into the first Cluster. Every
/// Cluster after the first then keeps the offset it already has.
pub fn plan(
    head: &Head,
    patch: &ColourPatch,
    extra_shrink: i64,
    default_duration: Option<u64>,
) -> Result<HeadPatch> {
    let buf = &head.buf;
    let new_colour = colour::build(buf, head.video.colour.as_ref(), patch)?;
    let old_colour_len = head.video.colour.map(|e| e.total_len()).unwrap_or(0);
    // Size fields may widen: whatever the Tracks element grows by, header
    // bytes included, comes out of a Void below.
    let new_tracks = rebuild_tracks(head, &new_colour, None, false, DvMapping::Keep, default_duration)?;
    let tracks_el = *ebml::find(&head.top, ebml::ID_TRACKS).context("no Tracks element")?;
    // Everything the Tracks element grew by has to come out of a Void: the
    // Colour, and a DefaultDuration added to a track that had none.
    let delta = new_tracks.len() as i64 - tracks_el.total_len() as i64;
    let _ = old_colour_len;
    let needed = delta + extra_shrink;

    // Pick the Void that has to absorb the difference. Prefer one next to what
    // changed: then nothing between them has to move, and the SeekHead and the
    // Cues stay valid as they are.
    let tracks_index = head
        .top
        .iter()
        .position(|e| e.id == ebml::ID_TRACKS)
        .context("no Tracks element")?;
    let mut candidates: Vec<(usize, Elem)> = head
        .top
        .iter()
        .enumerate()
        .filter(|(_, e)| e.id == ebml::ID_VOID)
        .map(|(i, e)| (i, *e))
        .collect();
    candidates.sort_by_key(|(i, e)| {
        let between = if *i > tracks_index {
            *i - tracks_index - 1
        } else {
            tracks_index - *i - 1
        };
        // Fewest displaced elements first, then the roomiest.
        (between, std::cmp::Reverse(e.size))
    });

    let mut chosen: Option<Elem> = None;
    if needed != 0 {
        for (_, void) in &candidates {
            let after = void.size as i64 - needed;
            if after < 0 {
                continue;
            }
            // A Void's own size field must still hold the new payload.
            if ebml::min_size_width(after as u64) > void.size_len {
                continue;
            }
            chosen = Some(*void);
            break;
        }
        if chosen.is_none() {
            let biggest = candidates.iter().map(|(_, e)| e.size).max().unwrap_or(0);
            bail!(
                "the change needs {needed} spare bytes in the head and the largest Void \
                 holds {biggest}. The file would have to be rewritten instead of patched."
            );
        }
    }

    // Emit the head, remembering where every top-level element ended up.
    let mut out = Vec::with_capacity(head.head_end);
    out.extend_from_slice(&buf[..head.segment_body]);
    let mut new_offset: HashMap<usize, usize> = HashMap::new();
    for el in &head.top {
        new_offset.insert(el.offset, out.len());
        if Some(el.offset) == chosen.map(|v| v.offset) {
            let payload = (el.size as i64 - needed) as u64;
            ebml::write_id(ebml::ID_VOID, &mut out);
            ebml::write_size_at(payload, el.size_len, &mut out)?;
            out.resize(out.len() + payload as usize, 0);
        } else if el.offset == tracks_el.offset {
            out.extend_from_slice(&new_tracks);
        } else {
            out.extend_from_slice(&buf[el.offset..el.body_end()]);
        }
    }

    let want = head.head_end as i64 - extra_shrink;
    if out.len() as i64 != want {
        bail!(
            "internal error: rebuilt head is {} bytes, expected {want}",
            out.len()
        );
    }

    // Correct the SeekHead for anything it points at that moved. Elements the
    // index does not reference - Void padding above all - simply move.
    let mut target: HashMap<u32, (u64, u64)> = HashMap::new();
    for el in &head.top {
        let old = head.seek_pos(el.offset);
        let new = (new_offset[&el.offset] - head.segment_body) as u64;
        target.entry(el.id).or_insert((old, new));
    }
    let moved = match ebml::find(&head.top, ebml::ID_SEEK_HEAD) {
        Some(sh) => {
            let sh_new = new_offset[&sh.offset];
            patch_seek_head(&mut out, sh, sh_new, &target)?
        }
        None => Vec::new(),
    };

    // When the head shrinks, everything from the first difference to its new end
    // has to go out, because the Cluster that follows moves with it.
    let (write_offset, write_len) = if extra_shrink != 0 {
        let common = out.len().min(head.head_end);
        let first = buf[..common]
            .iter()
            .zip(&out)
            .position(|(a, b)| a != b)
            .unwrap_or(common);
        (first, out.len() - first)
    } else {
        diff_range(&buf[..head.head_end], &out)
    };

    Ok(HeadPatch {
        new_head: out,
        delta,
        void_offset: chosen.map(|v| v.offset),
        void_before: chosen.map(|v| v.size).unwrap_or(0),
        void_after: chosen.map(|v| (v.size as i64 - needed) as u64).unwrap_or(0),
        write_offset,
        write_len,
        moved,
    })
}

/// Rewrite SeekPosition bodies at their existing width, in the rebuilt buffer.
fn patch_seek_head(
    out: &mut [u8],
    old_sh: &Elem,
    new_sh_offset: usize,
    target: &HashMap<u32, (u64, u64)>,
) -> Result<Vec<(u32, u64, u64)>> {
    // The SeekHead kept its length, so offsets inside it just shift as a block.
    let shift = new_sh_offset as i64 - old_sh.offset as i64;
    let body = (old_sh.body as i64 + shift) as usize;
    let body_end = (old_sh.body_end() as i64 + shift) as usize;
    let mut corrected = Vec::new();
    let seeks = ebml::children(out, body, body_end)?;
    for seek in seeks.iter().filter(|e| e.id == ebml::ID_SEEK) {
        let kids = ebml::children(out, seek.body, seek.body_end())?;
        let (Some(id_el), Some(pos_el)) = (
            ebml::find(&kids, ebml::ID_SEEK_ID),
            ebml::find(&kids, ebml::ID_SEEK_POSITION),
        ) else {
            continue;
        };
        let mut seek_id: u32 = 0;
        for i in 0..id_el.size as usize {
            seek_id = (seek_id << 8) | out[id_el.body + i] as u32;
        }
        let Some(&(old_pos, new_pos)) = target.get(&seek_id) else {
            continue; // points past the head - Cues and Tags do not move
        };
        let stored = ebml::read_uint(out, pos_el)?;
        if stored != old_pos {
            bail!(
                "SeekHead says element {seek_id:#X} is at {stored}, but it is at {old_pos}; \
                 refusing to patch a file whose index is already inconsistent"
            );
        }
        if new_pos == old_pos {
            continue;
        }
        let mut enc = Vec::new();
        ebml::write_uint_at(new_pos, pos_el.size as usize, &mut enc).with_context(|| {
            format!(
                "the new position {new_pos} of element {seek_id:#X} does not fit in the \
                 {} byte SeekPosition the file reserved",
                pos_el.size
            )
        })?;
        out[pos_el.body..pos_el.body_end()].copy_from_slice(&enc);
        corrected.push((seek_id, old_pos, new_pos));
    }
    Ok(corrected)
}

/// First and last differing byte, so we write the least we can get away with.
pub fn diff_range(old: &[u8], new: &[u8]) -> (usize, usize) {
    debug_assert_eq!(old.len(), new.len());
    let first = old.iter().zip(new).position(|(a, b)| a != b);
    let Some(first) = first else {
        return (0, 0);
    };
    let last = old
        .iter()
        .zip(new)
        .rposition(|(a, b)| a != b)
        .expect("a first difference implies a last one");
    (first, last - first + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_range_finds_the_narrow_window() {
        let a = vec![1u8, 2, 3, 4, 5, 6];
        let mut b = a.clone();
        b[2] = 9;
        b[4] = 9;
        assert_eq!(diff_range(&a, &b), (2, 3));
        assert_eq!(diff_range(&a, &a), (0, 0));
    }
}
