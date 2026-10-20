//! Writing a Matroska file in a single forward pass.
//!
//! The awkward part of writing Matroska is that the SeekHead at the front has to
//! name positions that are only known once everything is written, and the
//! Segment has to state a length that is only known at the very end. Rather than
//! buffer the file or walk it twice, both are written as fixed-width
//! placeholders and filled in with a handful of small seeks at the close.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use crate::ebml;

/// A SeekPosition reserved in the SeekHead, and the value we later put there.
struct Slot {
    id: u32,
    /// Absolute file offset of the reserved field.
    at: u64,
    value: Option<u64>,
}

/// Positions and lengths are reserved at their widest, so filling them in later
/// never changes anyone's length.
const WIDE: usize = 8;

pub struct Writer {
    out: BufWriter<File>,
    /// Absolute offset of the Segment body; every stored position is relative
    /// to it.
    segment_body: u64,
    /// Where the Segment's own size field sits, to be filled in at the end.
    segment_size_at: u64,
    seek_slots: Vec<Slot>,
    pos: u64,
}

fn write_elem_header(out: &mut impl Write, id: u32, size: u64, width: usize) -> Result<usize> {
    let mut hdr = Vec::new();
    ebml::write_id(id, &mut hdr);
    ebml::write_size_at(size, width, &mut hdr)?;
    out.write_all(&hdr)?;
    Ok(hdr.len())
}

impl Writer {
    /// Start a file, copying the source's EBML header verbatim.
    pub fn create(path: &Path, ebml_header: &[u8], seek_ids: &[u32]) -> Result<Writer> {
        let file = File::create(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let mut out = BufWriter::with_capacity(4 << 20, file);
        out.write_all(ebml_header)?;
        let mut pos = ebml_header.len() as u64;

        // Segment, with a length we come back for.
        let mut hdr = Vec::new();
        ebml::write_id(ebml::ID_SEGMENT, &mut hdr);
        ebml::write_size_at(0, WIDE, &mut hdr)?;
        out.write_all(&hdr)?;
        let segment_size_at = pos + (hdr.len() - WIDE) as u64;
        pos += hdr.len() as u64;
        let segment_body = pos;

        // A SeekHead with one slot per element we will write, each position
        // reserved at full width so filling it in cannot shift anything.
        let mut body = Vec::new();
        let mut slots = Vec::new();
        for id in seek_ids {
            let mut id_bytes = Vec::new();
            ebml::write_id(*id, &mut id_bytes);
            let mut seek = Vec::new();
            ebml::write_id(ebml::ID_SEEK_ID, &mut seek);
            ebml::write_size_at(id_bytes.len() as u64, 1, &mut seek)?;
            seek.extend_from_slice(&id_bytes);
            ebml::write_id(ebml::ID_SEEK_POSITION, &mut seek);
            ebml::write_size_at(WIDE as u64, 1, &mut seek)?;
            seek.extend_from_slice(&[0u8; WIDE]);

            ebml::write_id(ebml::ID_SEEK, &mut body);
            ebml::write_size_at(seek.len() as u64, 1, &mut body)?;
            body.extend_from_slice(&seek);
            // The placeholder is the tail of what was just appended. Measuring
            // it from the end cannot drift when a header in front of it changes.
            slots.push((*id, (body.len() - WIDE) as u64));
        }
        let hdr_len = write_elem_header(&mut out, ebml::ID_SEEK_HEAD, body.len() as u64, 2)?;
        let body_at = pos + hdr_len as u64;
        out.write_all(&body)?;
        pos += hdr_len as u64 + body.len() as u64;

        let seek_slots = slots
            .into_iter()
            .map(|(id, rel)| Slot {
                id,
                at: body_at + rel,
                value: None,
            })
            .collect();

        Ok(Writer {
            out,
            segment_body,
            segment_size_at,
            seek_slots,
            pos,
        })
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Position relative to the Segment body, the way Matroska records it.
    pub fn segment_relative(&self) -> u64 {
        self.pos - self.segment_body
    }

    /// Note that the element with this ID starts here, for the SeekHead.
    pub fn mark(&mut self, id: u32) {
        let here = self.segment_relative();
        if let Some(slot) = self.seek_slots.iter_mut().find(|s| s.id == id) {
            slot.value = Some(here);
        }
    }

    pub fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    pub fn write_element(&mut self, id: u32, body: &[u8]) -> Result<()> {
        let width = ebml::min_size_width(body.len() as u64);
        let n = write_elem_header(&mut self.out, id, body.len() as u64, width)?;
        self.out.write_all(body)?;
        self.pos += n as u64 + body.len() as u64;
        Ok(())
    }

    /// Overwrite bytes already written, leaving the write position where it
    /// is - for a value that is only known once the Clusters are through.
    pub fn patch_at(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        if at + bytes.len() as u64 > self.pos {
            bail!("patching {} bytes at {at} would run past what was written", bytes.len());
        }
        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(at))?;
        file.write_all(bytes)?;
        file.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }

    /// Fill in the Segment length and every SeekHead slot, then close.
    pub fn finish(mut self) -> Result<()> {
        let segment_len = self.pos - self.segment_body;
        let mut size = Vec::new();
        ebml::write_size_at(segment_len, WIDE, &mut size)?;

        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(self.segment_size_at))?;
        file.write_all(&size)?;

        for slot in &self.seek_slots {
            // An element we never wrote keeps its zero, which no reader follows.
            let Some(value) = slot.value else { continue };
            let mut enc = Vec::new();
            ebml::write_uint_at(value, WIDE, &mut enc)?;
            file.seek(SeekFrom::Start(slot.at))?;
            file.write_all(&enc)?;
        }
        file.sync_all()?;
        Ok(())
    }
}

/// One entry of the index we rebuild as Clusters are written.
pub struct CuePoint {
    pub time: u64,
    pub track: u64,
    pub cluster_position: u64,
    pub relative_position: u64,
}

/// Build the Cues element from the points collected during the pass.
pub fn build_cues(points: &[CuePoint]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for p in points {
        let mut positions = Vec::new();
        emit_uint(0xF7, p.track, &mut positions)?; // CueTrack
        emit_uint(0xF1, p.cluster_position, &mut positions)?; // CueClusterPosition
        emit_uint(0xF0, p.relative_position, &mut positions)?; // CueRelativePosition

        let mut point = Vec::new();
        emit_uint(0xB3, p.time, &mut point)?; // CueTime
        emit_master(0xB7, &positions, &mut point)?; // CueTrackPositions
        emit_master(0xBB, &point, &mut body)?; // CuePoint
    }
    Ok(body)
}

pub fn emit_uint(id: u32, value: u64, out: &mut Vec<u8>) -> Result<()> {
    let width = ebml::min_uint_width(value);
    ebml::write_id(id, out);
    ebml::write_size_at(width as u64, 1, out)?;
    ebml::write_uint_at(value, width, out)
}

pub fn emit_master(id: u32, body: &[u8], out: &mut Vec<u8>) -> Result<()> {
    ebml::write_id(id, out);
    ebml::write_size_at(body.len() as u64, ebml::min_size_width(body.len() as u64), out)?;
    out.extend_from_slice(body);
    Ok(())
}

/// Wrap a body in an element header and return the bytes.
#[cfg(test)]
pub fn element(id: u32, body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len() + 12);
    emit_master(id, body, &mut out)?;
    Ok(out)
}

/// Guard against a Cluster that grew past what a Matroska size field can hold.
pub fn check_cluster_size(len: u64) -> Result<()> {
    if len >= (1u64 << 56) - 1 {
        bail!("a Cluster grew beyond what a size field can express");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reserved_position_is_wide_enough_for_a_large_file() {
        // The Cues of a seven gigabyte file sit far past what a narrow field
        // could hold, which is why every slot is reserved at full width.
        let mut enc = Vec::new();
        ebml::write_uint_at(6_750_589_420, WIDE, &mut enc).unwrap();
        assert_eq!(enc.len(), WIDE);
        let el = ebml::Elem {
            id: ebml::ID_SEEK_POSITION,
            id_len: 2,
            size: WIDE as u64,
            size_len: 1,
            offset: 0,
            body: 0,
            unknown_size: false,
        };
        assert_eq!(ebml::read_uint(&enc, &el).unwrap(), 6_750_589_420);
    }

    #[test]
    fn the_seek_head_reads_back_with_the_positions_that_were_marked() {
        // Writing the placeholders is easy to get subtly wrong: the value has
        // to land exactly on the reserved bytes, not on the header before them.
        let path = std::env::temp_dir().join(format!(
            "ddvt_patch_seekhead_{}.mkv",
            std::process::id()
        ));
        let ebml_header = [0x1A, 0x45, 0xDF, 0xA3, 0x80];
        let ids = [ebml::ID_INFO, ebml::ID_TRACKS, ebml::ID_CUES];
        let mut w = Writer::create(&path, &ebml_header, &ids).unwrap();
        w.mark(ebml::ID_INFO);
        w.write_element(ebml::ID_INFO, &[0xAA; 30]).unwrap();
        let tracks_at = w.segment_relative();
        w.mark(ebml::ID_TRACKS);
        w.write_element(ebml::ID_TRACKS, &[0xBB; 700]).unwrap();
        let cues_at = w.segment_relative();
        w.mark(ebml::ID_CUES);
        w.write_element(ebml::ID_CUES, &[0xCC; 5]).unwrap();
        let end = w.position();
        w.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes.len() as u64, end);

        let seg = ebml::read_elem(&bytes, ebml_header.len()).unwrap();
        assert_eq!(seg.id, ebml::ID_SEGMENT);
        assert_eq!(seg.body_end(), bytes.len(), "Segment length filled in");

        let top = ebml::children(&bytes, seg.body, seg.body_end()).unwrap();
        let sh = top[0];
        assert_eq!(sh.id, ebml::ID_SEEK_HEAD);
        let mut found = Vec::new();
        for seek in ebml::children(&bytes, sh.body, sh.body_end()).unwrap() {
            assert_eq!(seek.id, ebml::ID_SEEK);
            let kids = ebml::children(&bytes, seek.body, seek.body_end()).unwrap();
            let id_el = ebml::find(&kids, ebml::ID_SEEK_ID).expect("SeekID intact");
            let pos_el = ebml::find(&kids, ebml::ID_SEEK_POSITION).expect("SeekPosition intact");
            assert_eq!(pos_el.size as usize, WIDE);
            let mut id = 0u32;
            for i in 0..id_el.size as usize {
                id = (id << 8) | bytes[id_el.body + i] as u32;
            }
            found.push((id, ebml::read_uint(&bytes, pos_el).unwrap()));
        }
        assert_eq!(found[1], (ebml::ID_TRACKS, tracks_at));
        assert_eq!(found[2], (ebml::ID_CUES, cues_at));
        // And the positions really point at those elements.
        let tracks = ebml::read_elem(&bytes, seg.body + tracks_at as usize).unwrap();
        assert_eq!(tracks.id, ebml::ID_TRACKS);
        assert_eq!(tracks.size, 700);
    }

    #[test]
    fn cues_round_trip_through_the_parser() {
        let points = vec![
            CuePoint {
                time: 0,
                track: 1,
                cluster_position: 6951,
                relative_position: 10,
            },
            CuePoint {
                time: 6715,
                track: 1,
                cluster_position: 1_848_105,
                relative_position: 5,
            },
        ];
        let body = build_cues(&points).unwrap();
        let cues = element(ebml::ID_CUES, &body).unwrap();

        let el = ebml::read_elem(&cues, 0).unwrap();
        assert_eq!(el.id, ebml::ID_CUES);
        let kids = ebml::children(&cues, el.body, el.body_end()).unwrap();
        assert_eq!(kids.len(), 2);

        let inner = ebml::children(&cues, kids[1].body, kids[1].body_end()).unwrap();
        let time = ebml::find(&inner, 0xB3).unwrap();
        assert_eq!(ebml::read_uint(&cues, time).unwrap(), 6715);
        let tp = ebml::find(&inner, 0xB7).unwrap();
        let tpk = ebml::children(&cues, tp.body, tp.body_end()).unwrap();
        let pos = ebml::find(&tpk, 0xF1).unwrap();
        assert_eq!(ebml::read_uint(&cues, pos).unwrap(), 1_848_105);
    }
}
