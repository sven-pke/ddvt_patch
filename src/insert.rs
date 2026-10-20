//! Making the video stream longer without making the file longer.
//!
//! A metadata OBU that is missing cannot be written over something else - it has
//! to be inserted, and every byte inserted normally pushes the rest of the file
//! along. The way out is that Matroska heads carry Void padding: take the bytes
//! from there, and the first Cluster grows by exactly as much as the head
//! shrinks. Every later Cluster keeps the offset it already had, so only the
//! head, the first Cluster and a couple of Cue entries have to be written.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::ebml::{self, Elem};
use crate::mkv::{Codec, Head};

/// Build an AV1 metadata OBU, trailing bits and all.
///
/// The trailing byte is not decoration: without it ffmpeg rejects the whole
/// stream, because every OBU except tile groups has to end byte aligned.
pub fn av1_metadata_obu(metadata_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = vec![metadata_type];
    inner.extend_from_slice(payload);
    inner.push(0x80); // trailing_one_bit plus padding

    let mut out = Vec::with_capacity(inner.len() + 3);
    out.push((5u8 << 3) | 0x02); // OBU_METADATA, has_size_field
    let mut len = inner.len();
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(&inner);
    out
}

pub(crate) fn leb128(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for i in 0..8 {
        let b = *buf.get(pos + i)?;
        value |= ((b & 0x7F) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Where a new metadata OBU belongs inside a temporal unit.
///
/// After the temporal delimiter, the sequence header and any metadata already
/// there; before the first OBU that carries picture data. Putting it after the
/// picture data would leave it ignored, putting it first can make a muxer hoist
/// it into CodecPrivate.
pub fn metadata_insert_index(frame: &[u8]) -> usize {
    let mut pos = 0usize;
    while pos < frame.len() {
        let header = frame[pos];
        let obu_type = (header >> 3) & 0x0F;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut p = pos + 1;
        if extension {
            p += 1;
        }
        let (payload_len, payload_start) = if has_size {
            match leb128(frame, p) {
                Some((len, n)) => (len as usize, p + n),
                None => return pos,
            }
        } else {
            (frame.len() - p, p)
        };
        let end = payload_start.saturating_add(payload_len);
        if end > frame.len() {
            return pos;
        }
        // 3 frame header, 4 tile group, 6 frame, 7 redundant frame header.
        if matches!(obu_type, 3 | 4 | 6 | 7) {
            return pos;
        }
        if !has_size {
            return frame.len();
        }
        pos = end;
    }
    frame.len()
}

pub struct ClusterInsert {
    pub old_len: usize,
    pub new_cluster: Vec<u8>,
    /// How many bytes the Cluster grew, and so how many the head must give up.
    pub delta: i64,
}

/// Splice OBUs into the first video frame of the first Cluster.
pub fn plan_first_cluster_insert(
    path: &Path,
    head: &Head,
    obus: &[u8],
    overwrites: &[(u64, Vec<u8>)],
) -> Result<ClusterInsert> {
    if head.video.codec != Codec::Av1 {
        bail!("inserting metadata units is implemented for AV1 only");
    }
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(head.head_end as u64))?;

    let mut header = [0u8; 12];
    f.read_exact(&mut header)?;
    let cluster = ebml::read_elem(&header, 0)?;
    if cluster.id != ebml::ID_CLUSTER {
        bail!("the first element after the head is not a Cluster");
    }
    if cluster.unknown_size {
        bail!("the first Cluster has an unknown size; this file needs a full rewrite");
    }
    let total = cluster.total_len();
    f.seek(SeekFrom::Start(head.head_end as u64))?;
    let mut buf = vec![0u8; total];
    f.read_exact(&mut buf)?;

    // Corrections that land inside this Cluster are applied before it is rebuilt,
    // because afterwards their offsets no longer mean anything.
    for (offset, bytes) in overwrites {
        let rel = offset
            .checked_sub(head.head_end as u64)
            .filter(|r| *r as usize + bytes.len() <= buf.len())
            .context("an overwrite was routed to the wrong Cluster")? as usize;
        buf[rel..rel + bytes.len()].copy_from_slice(bytes);
    }

    // Find the first block belonging to the video track.
    let kids = ebml::children(&buf, cluster.body, cluster.body_end())?;
    if kids.iter().any(|e| e.id == ebml::ID_CRC32) {
        bail!(
            "the first Cluster carries a CRC-32 that a splice would leave stale; \
             this file needs a rewrite, which drops the checksums"
        );
    }
    let mut target: Option<(Elem, Elem)> = None; // (containing element, block)
    for kid in &kids {
        match kid.id {
            0xA3 => {
                if block_track(&buf, kid) == Some(head.video.track_number) {
                    target = Some((*kid, *kid));
                    break;
                }
            }
            0xA0 => {
                let inner = ebml::children(&buf, kid.body, kid.body_end())?;
                if let Some(block) = inner.iter().find(|e| e.id == 0xA1) {
                    if block_track(&buf, block) == Some(head.video.track_number) {
                        target = Some((*kid, *block));
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    let (_outer, block) = target.context("the first Cluster holds no video block")?;

    // Only unlaced blocks, which is what every muxer writes for video.
    let b0 = buf[block.body];
    let vint_len = b0.leading_zeros() as usize + 1;
    let flags = buf[block.body + vint_len + 2];
    if (flags >> 1) & 0x03 != 0 {
        bail!("the first video block uses lacing; this patcher does not splice laced blocks");
    }
    let frame_start = block.body + vint_len + 3;
    let frame = &buf[frame_start..block.body_end()];
    let at = metadata_insert_index(frame);

    // Rebuild the block, then the Cluster around it.
    let mut block_body = Vec::with_capacity(block.size as usize + obus.len());
    block_body.extend_from_slice(&buf[block.body..frame_start]);
    block_body.extend_from_slice(&frame[..at]);
    block_body.extend_from_slice(obus);
    block_body.extend_from_slice(&frame[at..]);

    let mut new_block = Vec::new();
    ebml::write_id(block.id, &mut new_block);
    let width = block
        .size_len
        .max(ebml::min_size_width(block_body.len() as u64));
    ebml::write_size_at(block_body.len() as u64, width, &mut new_block)?;
    new_block.extend_from_slice(&block_body);

    // Splice the new block back in. When it sits inside a BlockGroup that group
    // has to grow too, so rebuild by byte ranges rather than by element.
    let mut inner = Vec::new();
    inner.extend_from_slice(&buf[cluster.body..block.offset]);
    inner.extend_from_slice(&new_block);
    inner.extend_from_slice(&buf[block.body_end()..cluster.body_end()]);
    let grew = new_block.len() as i64 - block.total_len() as i64;
    if _outer.id == 0xA0 {
        // Fix the BlockGroup's own size, which the byte splice above left alone.
        let rel = _outer.offset - cluster.body;
        let group = ebml::read_elem(&inner, rel)?;
        let new_size = (group.size as i64 + grew) as u64;
        let mut hdr = Vec::new();
        ebml::write_id(group.id, &mut hdr);
        ebml::write_size_at(new_size, group.size_len, &mut hdr).context(
            "the BlockGroup would need a wider size field than the file reserved",
        )?;
        if hdr.len() != group.header_len() {
            bail!("the BlockGroup header changed length; not supported");
        }
        inner[rel..rel + hdr.len()].copy_from_slice(&hdr);
    }

    let mut new_cluster = Vec::with_capacity(inner.len() + 12);
    ebml::write_id(cluster.id, &mut new_cluster);
    let cwidth = cluster
        .size_len
        .max(ebml::min_size_width(inner.len() as u64));
    ebml::write_size_at(inner.len() as u64, cwidth, &mut new_cluster)?;
    new_cluster.extend_from_slice(&inner);

    Ok(ClusterInsert {
        old_len: total,
        delta: new_cluster.len() as i64 - total as i64,
        new_cluster,
    })
}

fn block_track(buf: &[u8], block: &Elem) -> Option<u64> {
    ebml::read_vint(buf, block.body).map(|(v, _)| v)
}

/// Total byte length of the first Cluster, header included.
pub fn first_cluster_len(path: &Path, head: &Head) -> Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(head.head_end as u64))?;
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr)?;
    let cluster = ebml::read_elem(&hdr, 0)?;
    if cluster.id != ebml::ID_CLUSTER || cluster.unknown_size {
        bail!("the first element after the head is not a sized Cluster");
    }
    Ok(cluster.total_len() as u64)
}

/// Correct the Cue entries that point at a Cluster which moved.
///
/// CueRelativePosition is measured from the Cluster, so it stays as it is; only
/// CueClusterPosition changes, and it is a plain integer we can overwrite.
pub fn cue_fixups(
    path: &Path,
    head: &Head,
    old_pos: u64,
    new_pos: u64,
) -> Result<Vec<(u64, Vec<u8>)>> {
    if old_pos == new_pos {
        return Ok(Vec::new());
    }
    let Some(cues_at) = seek_head_target(head, ebml::ID_CUES)? else {
        return Ok(Vec::new());
    };
    let absolute = head.segment_body as u64 + cues_at;
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(absolute))?;
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr)?;
    let cues = ebml::read_elem(&hdr, 0)?;
    if cues.id != ebml::ID_CUES || cues.unknown_size {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(absolute))?;
    let mut buf = vec![0u8; cues.total_len()];
    f.read_exact(&mut buf)?;

    let mut out = Vec::new();
    for point in ebml::children(&buf, cues.body, cues.body_end())?
        .iter()
        .filter(|e| e.id == 0xBB)
    {
        for tp in ebml::children(&buf, point.body, point.body_end())?
            .iter()
            .filter(|e| e.id == 0xB7)
        {
            for pos_el in ebml::children(&buf, tp.body, tp.body_end())?
                .iter()
                .filter(|e| e.id == 0xF1)
            {
                if ebml::read_uint(&buf, pos_el)? != old_pos {
                    continue;
                }
                let mut enc = Vec::new();
                ebml::write_uint_at(new_pos, pos_el.size as usize, &mut enc).context(
                    "the corrected Cue position does not fit in the field the file reserved",
                )?;
                out.push((absolute + pos_el.body as u64, enc));
            }
        }
    }
    Ok(out)
}

/// Segment-relative position of a top level element, from the SeekHead.
pub(crate) fn seek_head_target(head: &Head, id: u32) -> Result<Option<u64>> {
    let Some(sh) = ebml::find(&head.top, ebml::ID_SEEK_HEAD) else {
        return Ok(None);
    };
    let buf = &head.buf;
    for seek in ebml::children(buf, sh.body, sh.body_end())?
        .iter()
        .filter(|e| e.id == ebml::ID_SEEK)
    {
        let kids = ebml::children(buf, seek.body, seek.body_end())?;
        let (Some(id_el), Some(pos_el)) = (
            ebml::find(&kids, ebml::ID_SEEK_ID),
            ebml::find(&kids, ebml::ID_SEEK_POSITION),
        ) else {
            continue;
        };
        let mut seek_id: u32 = 0;
        for i in 0..id_el.size as usize {
            seek_id = (seek_id << 8) | buf[id_el.body + i] as u32;
        }
        if seek_id == id {
            return Ok(Some(ebml::read_uint(buf, pos_el)?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_light_level_obu_is_eight_bytes() {
        let obu = av1_metadata_obu(1, &[0x06, 0xE8, 0x01, 0xF9]);
        // header, size, metadata_type, four payload bytes, trailing bits.
        assert_eq!(obu.len(), 8);
        assert_eq!(obu[0], 0x2A); // OBU_METADATA with has_size_field
        assert_eq!(obu[1], 6);
        assert_eq!(obu[2], 1); // metadata_type = HDR_CLL
        assert_eq!(*obu.last().unwrap(), 0x80);
    }

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    /// Frames with the extension header, as a stream with temporal or spatial
    /// layers carries them: the new metadata still goes right before them.
    #[test]
    fn metadata_goes_before_a_frame_that_carries_the_extension_header() {
        let mut frame = obu(2, &[]); // temporal delimiter
        frame.extend_from_slice(&obu(1, &[0xAA, 0xBB])); // sequence header
        // metadata with extension: header 0x2E, extension, size, payload
        frame.extend_from_slice(&[(5 << 3) | 0x06, 0x20, 3, 2, 0, 0]);
        let picture_at = frame.len();
        // frame with extension
        frame.extend_from_slice(&[(6 << 3) | 0x06, 0x48, 4, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(metadata_insert_index(&frame), picture_at);
    }

    #[test]
    fn metadata_goes_after_the_existing_metadata_and_before_the_picture() {
        let mut frame = obu(2, &[]); // temporal delimiter
        frame.extend_from_slice(&obu(1, &[0xAA, 0xBB])); // sequence header
        frame.extend_from_slice(&obu(5, &[2, 0, 0])); // existing metadata
        let expected = frame.len();
        frame.extend_from_slice(&obu(6, &[0u8; 16])); // the frame itself
        assert_eq!(metadata_insert_index(&frame), expected);
    }

    #[test]
    fn a_frame_with_no_picture_data_takes_the_metadata_at_the_end() {
        let frame = obu(2, &[]);
        assert_eq!(metadata_insert_index(&frame), frame.len());
    }

    #[test]
    fn a_frame_that_starts_with_picture_data_takes_it_at_the_front() {
        let frame = obu(6, &[0u8; 4]);
        assert_eq!(metadata_insert_index(&frame), 0);
    }

    #[test]
    fn inserting_keeps_every_other_obu_byte_identical() {
        let mut frame = obu(2, &[]);
        frame.extend_from_slice(&obu(1, &[0xAA, 0xBB]));
        frame.extend_from_slice(&obu(6, &[0x11; 8]));
        let at = metadata_insert_index(&frame);
        let cll = av1_metadata_obu(1, &[0x06, 0xE8, 0x01, 0xF9]);

        let mut spliced = frame[..at].to_vec();
        spliced.extend_from_slice(&cll);
        spliced.extend_from_slice(&frame[at..]);

        assert_eq!(spliced.len(), frame.len() + cll.len());
        assert_eq!(&spliced[..at], &frame[..at]);
        assert_eq!(&spliced[at + cll.len()..], &frame[at..]);
    }
}
