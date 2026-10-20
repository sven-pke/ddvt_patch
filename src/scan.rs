//! Walk the Clusters and find the metadata that sits inside the video itself.
//!
//! The container is only half the story: an AV1 stream carries its own HDR_CLL
//! and HDR_MDCV metadata OBUs, and HEVC carries the same values as SEI messages.
//! Players prefer those over the container, so a patcher has to see them.
//!
//! Every item is recorded with the absolute file offset of its payload, because
//! those payloads are fixed size - which is what lets us overwrite them in place.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::ebml::{self};
use crate::mkv::{Codec, Head};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ItemKind {
    /// Mastering display colour volume. 24 byte payload.
    Mdcv,
    /// Content light level: MaxCLL and MaxFALL. 4 byte payload.
    Cll,
    /// Dolby Vision RPU.
    DolbyVisionRpu,
    /// SMPTE ST 2094-40 dynamic metadata.
    Hdr10Plus,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub kind: ItemKind,
    /// Absolute file offset of the patchable payload.
    pub offset: u64,
    /// Payload length in bytes.
    pub len: usize,
    pub frame: u64,
    /// Presentation timestamp of the frame, in container ticks.
    pub ts: u64,
    /// True when the bytes on disk are a verbatim copy of the payload, so an
    /// equal-length replacement can simply be written over them. HEVC turns this
    /// off when emulation prevention bytes sit inside the payload.
    pub patchable_in_place: bool,
    /// The clean payload, for the fixed-size kinds. On disk it may be escaped,
    /// so this is what a value must be decoded from.
    #[serde(skip)]
    pub payload: Option<Vec<u8>>,
}

impl Item {
    /// The payload as a value: the clean copy when the scan kept one, else the
    /// bytes on disk, which are verbatim for everything the scan did not clean.
    pub fn payload_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        if let Some(p) = &self.payload {
            return Ok(p.clone());
        }
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = vec![0u8; self.len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanResult {
    pub frames: u64,
    pub items: Vec<Item>,
    pub bytes_scanned: u64,
    /// False when the scan stopped at the limit rather than at the end.
    pub complete: bool,
    /// Earliest and latest video block timestamp seen, in container ticks.
    pub ts_min: Option<u64>,
    pub ts_max: Option<u64>,
    /// Video blocks whose timestamp equals the one before them in file
    /// order: a duplicated frame, or a muxer that lost the timing.
    pub ts_repeats: u64,
    prev_ts: Option<u64>,
    /// Every video timestamp in file order, in ticks, for the timing report.
    pub timestamps: Vec<i64>,
    /// The first timestamp seen for each of the other tracks (number, ticks).
    pub first_ts_by_track: Vec<(u64, i64)>,
    /// Dolby Vision enhancement layer units (HEVC NAL type 63): a profile 7
    /// stream that carries both layers in one track, the way discs do.
    pub el_units: u64,
}

impl ScanResult {
    pub fn count(&self, kind: ItemKind) -> usize {
        self.items.iter().filter(|i| i.kind == kind).count()
    }
    pub fn first(&self, kind: ItemKind) -> Option<&Item> {
        self.items.iter().find(|i| i.kind == kind)
    }
}

/// Read a LEB128 value, returning it and how many bytes it took.
fn leb128(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
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

/// Scan one AV1 temporal unit's OBUs.
fn scan_av1(frame: &[u8], base: u64, index: u64, ts: u64, collect: bool, out: &mut Vec<Item>) {
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
                None => return,
            }
        } else {
            (frame.len() - p, p)
        };
        let payload_end = payload_start.saturating_add(payload_len);
        if payload_end > frame.len() {
            return;
        }

        // OBU_METADATA
        if obu_type == 5 {
            if let Some((meta_type, n)) = leb128(frame, payload_start) {
                let body = payload_start + n;
                let avail = payload_end - body;
                let record = |kind: ItemKind, len: usize, out: &mut Vec<Item>| {
                    if avail >= len {
                        out.push(Item {
                            kind,
                            offset: base + body as u64,
                            len,
                            frame: index,
                            ts,
                            patchable_in_place: true,
                            payload: Some(frame[body..body + len].to_vec()),
                        });
                    }
                };
                match meta_type {
                    1 => record(ItemKind::Cll, 4, out),
                    2 => record(ItemKind::Mdcv, 24, out),
                    4 => {
                        // ITU-T T.35: country 0xB5, then a provider code.
                        let b = &frame[body..payload_end];
                        if b.len() >= 3 && b[0] == 0xB5 {
                            let provider = u16::from_be_bytes([b[1], b[2]]);
                            let kind = match provider {
                                0x003B => Some(ItemKind::DolbyVisionRpu),
                                0x003C => Some(ItemKind::Hdr10Plus),
                                _ => None,
                            };
                            if let Some(kind) = kind {
                                out.push(Item {
                                    kind,
                                    offset: base + body as u64,
                                    len: b.len(),
                                    frame: index,
                                    ts,
                                    patchable_in_place: false,
                                    payload: if collect { Some(b.to_vec()) } else { None },
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !has_size {
            return;
        }
        pos = payload_end;
    }
}

/// Strip emulation prevention bytes. Returns the clean bytes and, for each
/// clean byte, where it sits in the escaped input - so a payload found in the
/// clean stream can still be located on disk.
fn unescape(src: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut out = Vec::with_capacity(src.len());
    let mut map = Vec::with_capacity(src.len());
    let mut zeros = 0;
    for (i, &b) in src.iter().enumerate() {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
        map.push(i);
    }
    (out, map)
}

/// Scan one HEVC access unit stored as length-prefixed NAL units.
/// Returns how many enhancement layer units the frame holds.
fn scan_hevc(
    frame: &[u8],
    base: u64,
    index: u64,
    len_size: usize,
    ts: u64,
    collect: bool,
    out: &mut Vec<Item>,
) -> u64 {
    let mut el = 0;
    let mut pos = 0usize;
    while pos + len_size <= frame.len() {
        let mut nal_len = 0usize;
        for i in 0..len_size {
            nal_len = (nal_len << 8) | frame[pos + i] as usize;
        }
        let nal_start = pos + len_size;
        let nal_end = nal_start.saturating_add(nal_len);
        if nal_end > frame.len() || nal_len < 2 {
            return el;
        }
        let nal_type = (frame[nal_start] >> 1) & 0x3F;
        // 39 = prefix SEI. 62 is where Dolby puts the RPU, 63 the enhancement
        // layer.
        if nal_type == 63 {
            el += 1;
        } else if nal_type == 39 {
            scan_hevc_sei(
                &frame[nal_start + 2..nal_end],
                base + (nal_start + 2) as u64,
                index,
                ts,
                collect,
                out,
            );
        } else if nal_type == 62 {
            out.push(Item {
                kind: ItemKind::DolbyVisionRpu,
                offset: base + nal_start as u64,
                len: nal_len,
                frame: index,
                ts,
                patchable_in_place: false,
                payload: if collect {
                    Some(frame[nal_start..nal_end].to_vec())
                } else {
                    None
                },
            });
        }
        pos = nal_end;
    }
    el
}

/// Walk the messages of a prefix SEI. Sizes in an SEI count clean bytes, so
/// the parse runs on the unescaped stream and maps back to disk afterwards.
fn scan_hevc_sei(raw: &[u8], base: u64, index: u64, ts: u64, collect: bool, out: &mut Vec<Item>) {
    let (rbsp, map) = unescape(raw);
    let mut pos = 0usize;
    while pos < rbsp.len() {
        if rbsp[pos..].iter().all(|&b| b == 0x80 || b == 0) {
            return; // trailing bits
        }
        let mut payload_type = 0usize;
        while let Some(&b) = rbsp.get(pos) {
            pos += 1;
            payload_type += b as usize;
            if b != 0xFF {
                break;
            }
        }
        let mut payload_size = 0usize;
        while let Some(&b) = rbsp.get(pos) {
            pos += 1;
            payload_size += b as usize;
            if b != 0xFF {
                break;
            }
        }
        let end = pos.saturating_add(payload_size);
        if end > rbsp.len() {
            return;
        }
        let clean = &rbsp[pos..end];
        let kind = match payload_type {
            137 if clean.len() >= 24 => Some((ItemKind::Mdcv, 24)),
            144 if clean.len() >= 4 => Some((ItemKind::Cll, 4)),
            4 if clean.len() >= 3 && clean[0] == 0xB5 => {
                match u16::from_be_bytes([clean[1], clean[2]]) {
                    0x003C => Some((ItemKind::Hdr10Plus, clean.len())),
                    0x003B => Some((ItemKind::DolbyVisionRpu, clean.len())),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some((kind, len)) = kind {
            let fixed = matches!(kind, ItemKind::Mdcv | ItemKind::Cll);
            // On disk the payload is a verbatim copy only if no guard byte sits
            // inside it - then its escaped span is exactly as long as its value.
            let disk_start = map[pos];
            let contiguous = map[pos + len - 1] - disk_start == len - 1;
            out.push(Item {
                kind,
                offset: base + disk_start as u64,
                len,
                frame: index,
                ts,
                patchable_in_place: fixed && contiguous,
                payload: if fixed || collect {
                    Some(clean[..len].to_vec())
                } else {
                    None
                },
            });
        }
        pos = end;
    }
}

/// Split a Block body into its frames, honouring lacing.
fn block_frames(body: &[u8], body_offset: u64) -> Option<Vec<(u64, usize, usize)>> {
    // track number vint, int16 timestamp, flags
    let b0 = *body.first()?;
    let vint_len = b0.leading_zeros() as usize + 1;
    let mut p = vint_len + 2;
    let flags = *body.get(p)?;
    p += 1;
    let lacing = (flags >> 1) & 0x03;
    let mut out = Vec::new();
    if lacing == 0 {
        out.push((body_offset + p as u64, p, body.len() - p));
        return Some(out);
    }
    let count = *body.get(p)? as usize + 1;
    p += 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        2 => {
            // fixed size
            let each = (body.len() - p) / count;
            sizes = vec![each; count];
        }
        1 => {
            // Xiph
            for _ in 0..count - 1 {
                let mut n = 0usize;
                loop {
                    let b = *body.get(p)?;
                    p += 1;
                    n += b as usize;
                    if b != 0xFF {
                        break;
                    }
                }
                sizes.push(n);
            }
            sizes.push(body.len().checked_sub(p + sizes.iter().sum::<usize>())?);
        }
        3 => {
            // EBML: first size is a plain vint, the rest are signed deltas.
            let (first, n, _) = ebml::read_size(body, p).ok()?;
            p += n;
            sizes.push(first as usize);
            let mut prev = first as i64;
            for _ in 0..count.saturating_sub(2) {
                let (raw, n, _) = ebml::read_size(body, p).ok()?;
                p += n;
                let bias = (1i64 << (7 * n - 1)) - 1;
                prev += raw as i64 - bias;
                sizes.push(prev as usize);
            }
            if count >= 2 {
                sizes.push(body.len().checked_sub(p + sizes.iter().sum::<usize>())?);
            }
        }
        _ => return None,
    }
    for size in sizes {
        if p + size > body.len() {
            return None;
        }
        out.push((body_offset + p as u64, p, size));
        p += size;
    }
    Some(out)
}

/// Number of bytes each NAL length prefix uses, from the hvcC record.
fn hevc_length_size(head: &Head) -> Result<usize> {
    let cp = head
        .video
        .codec_private
        .context("HEVC track has no CodecPrivate, cannot tell the NAL length size")?;
    let b = &head.buf[cp.body..cp.body_end()];
    // hvcC: 21 bytes of fixed fields, then lengthSizeMinusOne in the low 2 bits.
    let byte = *b
        .get(21)
        .context("CodecPrivate is too short to be an hvcC record")?;
    Ok((byte & 0x03) as usize + 1)
}

/// Walk Clusters from the head end, stopping after `limit` bytes.
pub fn scan(path: &Path, head: &Head, limit: u64) -> Result<ScanResult> {
    scan_collect(path, head, limit, false)
}

/// The same walk, optionally keeping the bytes of every dynamic metadata unit
/// (RPU, HDR10+) so they can be written out again.
pub fn scan_collect(path: &Path, head: &Head, limit: u64, collect: bool) -> Result<ScanResult> {
    scan_progress(path, head, limit, collect, &mut |_, _| {})
}

/// The same walk, telling `progress` how far it is in bytes every few
/// Clusters - a reference read over a network share takes minutes, and
/// silence looks like a hang.
pub fn scan_progress(
    path: &Path,
    head: &Head,
    limit: u64,
    collect: bool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<ScanResult> {
    // Compressed or header-stripped tracks would need decoding before we could
    // find anything, and stripping changes offsets. Refuse rather than guess.
    let kids = ebml::children(&head.buf, head.video.track_entry.body, head.video.track_entry.body_end())?;
    if kids.iter().any(|e| e.id == 0x6D80) {
        bail!("the video track uses ContentEncodings (compression or header stripping); \
               this scanner needs the frames stored verbatim");
    }

    let len_size = match head.video.codec {
        Codec::Hevc => hevc_length_size(head)?,
        _ => 4,
    };
    let track = head.video.track_number;

    let mut file = File::open(path)?;
    let total = file.metadata()?.len();

    let mut result = ScanResult::default();
    let mut pos = head.head_end as u64;
    let stop = (head.head_end as u64).saturating_add(limit).min(total);
    let mut clusters: u64 = 0;

    // One read per Cluster: the body comes in whole and is walked in memory.
    // Going element by element through a buffered reader costs a buffer
    // refill for every seek, and over a network share that is the difference
    // between minutes and hours for the same file.
    let mut hdr = [0u8; 12];
    while pos < stop {
        file.seek(SeekFrom::Start(pos))?;
        let n = read_up_to(&mut file, &mut hdr)?;
        if n < 2 {
            break;
        }
        let el = match ebml::read_elem(&hdr[..n], 0) {
            Ok(e) => e,
            Err(_) => break,
        };
        if el.unknown_size {
            break;
        }
        let body_at = pos + el.header_len() as u64;
        if el.id == ebml::ID_CLUSTER {
            if el.size > (1u64 << 30) {
                bail!(
                    "the Cluster at {pos} is {} bytes, more than this scan buffers",
                    el.size
                );
            }
            let mut body = vec![0u8; el.size as usize];
            file.seek(SeekFrom::Start(body_at))?;
            file.read_exact(&mut body)?;
            scan_cluster(
                &body,
                body_at,
                track,
                head.video.codec,
                len_size,
                collect,
                &mut result,
            )?;
            clusters += 1;
            if clusters % 32 == 0 {
                progress(pos, stop);
            }
        }
        pos = body_at + el.size;
    }
    progress(pos.min(stop), stop);
    result.bytes_scanned = pos - head.head_end as u64;
    result.complete = pos >= total;
    Ok(result)
}

/// Walk one Cluster body: the timestamp, then every block of the video track,
/// on its own or inside a BlockGroup.
fn scan_cluster(
    body: &[u8],
    body_at: u64,
    track: u64,
    codec: Codec,
    len_size: usize,
    collect: bool,
    result: &mut ScanResult,
) -> Result<()> {
    let mut cluster_ts: u64 = 0;
    for kid in ebml::children(body, 0, body.len())? {
        if kid.unknown_size || kid.body_end() > body.len() {
            break;
        }
        match kid.id {
            0xE7 if kid.size <= 8 => cluster_ts = ebml::read_uint(body, &kid)?,
            0xA3 => handle_block(
                &body[kid.body..kid.body_end()],
                body_at + kid.body as u64,
                track,
                codec,
                len_size,
                cluster_ts,
                collect,
                result,
            )?,
            0xA0 => {
                for g in ebml::children(body, kid.body, kid.body_end())? {
                    if g.id == 0xA1 && !g.unknown_size && g.body_end() <= body.len() {
                        handle_block(
                            &body[g.body..g.body_end()],
                            body_at + g.body as u64,
                            track,
                            codec,
                            len_size,
                            cluster_ts,
                            collect,
                            result,
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// How many video frames the file holds, counted from its two ends: the
/// first and the last video timestamp over the frame duration. Reads the
/// first Cluster and the Clusters after the last Cue point - a few
/// megabytes, wherever the file is. The duration in the head is no
/// substitute: a muxer writes it on whatever grid it believed in at the
/// time, and a longer audio track stretches it.
///
/// The grid is measured from the first 16 MiB rather than taken from the
/// head, since the two disagree in the files this tool is for.
///
/// None when the file has no Cues to jump by, or too few frames to measure.
pub fn frames_from_ends(path: &Path, head: &Head) -> Result<Option<(u64, crate::retime::Measured)>> {
    let start = scan(path, head, 16 << 20)?;
    let Some(first) = start.ts_min else { return Ok(None) };
    let Some(measured) = crate::retime::Measured::from_scaled(
        &start.timestamps,
        head.video.default_duration_ns,
        head.timestamp_scale,
    ) else {
        return Ok(None);
    };
    let Some(last) = last_video_timestamp(path, head)? else { return Ok(None) };
    if last < first as i64 {
        return Ok(None);
    }
    Ok(Some((
        ((last - first as i64) as f64 / measured.ticks).round() as u64 + 1,
        measured,
    )))
}

/// The latest video timestamp in the file, in ticks, found by walking the
/// Clusters from the last Cue point to the end.
pub fn last_video_timestamp(path: &Path, head: &Head) -> Result<Option<i64>> {
    let Some(rel) = crate::insert::seek_head_target(head, ebml::ID_CUES)? else {
        return Ok(None);
    };
    let video = head.video.track_number;
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut hdr = [0u8; 12];

    // The last Cue point of the video track: where the last keyframe's
    // Cluster starts.
    let at = head.segment_body as u64 + rel;
    file.seek(SeekFrom::Start(at))?;
    let n = read_up_to(&mut file, &mut hdr)?;
    if n < 2 {
        return Ok(None);
    }
    let cues = ebml::read_elem(&hdr[..n], 0)?;
    if cues.id != ebml::ID_CUES || cues.unknown_size || cues.size > (64 << 20) {
        return Ok(None);
    }
    let mut body = vec![0u8; cues.size as usize];
    file.seek(SeekFrom::Start(at + cues.header_len() as u64))?;
    file.read_exact(&mut body)?;
    let mut start: Option<u64> = None;
    for point in ebml::children(&body, 0, body.len())?.iter().filter(|e| e.id == 0xBB) {
        for tp in ebml::children(&body, point.body, point.body_end())?.iter().filter(|e| e.id == 0xB7) {
            let inner = ebml::children(&body, tp.body, tp.body_end())?;
            let track = match inner.iter().find(|e| e.id == 0xF7) {
                Some(t) => Some(ebml::read_uint(&body, t)?),
                None => None,
            };
            if track.is_some() && track != Some(video) {
                continue;
            }
            if let Some(p) = inner.iter().find(|e| e.id == 0xF1) {
                let p = ebml::read_uint(&body, p)?;
                start = Some(start.map_or(p, |s| s.max(p)));
            }
        }
    }
    let Some(rel) = start else { return Ok(None) };

    // From there to the end, every video block.
    let mut pos = head.segment_body as u64 + rel;
    let mut last: Option<i64> = None;
    while pos < total {
        file.seek(SeekFrom::Start(pos))?;
        let n = read_up_to(&mut file, &mut hdr)?;
        if n < 2 {
            break;
        }
        let el = match ebml::read_elem(&hdr[..n], 0) {
            Ok(e) => e,
            Err(_) => break,
        };
        if el.unknown_size {
            break;
        }
        let body_at = pos + el.header_len() as u64;
        if el.id == ebml::ID_CLUSTER {
            if el.size > (1u64 << 30) {
                bail!("the Cluster at {pos} is {} bytes, more than this pass buffers", el.size);
            }
            let mut body = vec![0u8; el.size as usize];
            file.seek(SeekFrom::Start(body_at))?;
            file.read_exact(&mut body)?;
            let mut cluster_ts: i64 = 0;
            for kid in ebml::children(&body, 0, body.len())? {
                if kid.unknown_size || kid.body_end() > body.len() {
                    break;
                }
                match kid.id {
                    0xE7 if kid.size <= 8 => cluster_ts = ebml::read_uint(&body, &kid)? as i64,
                    0xA3 => note_video_ts(&body, &kid, video, cluster_ts, &mut last),
                    0xA0 => {
                        for g in ebml::children(&body, kid.body, kid.body_end())? {
                            if g.id == 0xA1 && !g.unknown_size && g.body_end() <= body.len() {
                                note_video_ts(&body, &g, video, cluster_ts, &mut last);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        pos = body_at + el.size;
    }
    Ok(last)
}

/// Keep the latest timestamp of the video track's blocks.
fn note_video_ts(body: &[u8], b: &ebml::Elem, video: u64, cluster_ts: i64, last: &mut Option<i64>) {
    let Some((num, vl)) = ebml::read_vint(body, b.body) else { return };
    if num != video {
        return;
    }
    if let Some(d) = body.get(b.body + vl..b.body + vl + 2) {
        let abs = cluster_ts + i16::from_be_bytes([d[0], d[1]]) as i64;
        *last = Some(last.map_or(abs, |l| l.max(abs)));
    }
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn handle_block(
    body: &[u8],
    body_offset: u64,
    track: u64,
    codec: Codec,
    len_size: usize,
    cluster_ts: u64,
    collect: bool,
    result: &mut ScanResult,
) -> Result<()> {
    let Some((num, vint_len)) = ebml::read_vint(body, 0) else {
        return Ok(());
    };
    let delta = match body.get(vint_len..vint_len + 2) {
        Some(d) => i16::from_be_bytes([d[0], d[1]]) as i64,
        None => 0,
    };
    let raw_ts = cluster_ts as i64 + delta;
    if num != track {
        if !result.first_ts_by_track.iter().any(|(n, _)| *n == num) {
            result.first_ts_by_track.push((num, raw_ts));
        }
        return Ok(());
    }
    result.timestamps.push(raw_ts);
    let ts = raw_ts.max(0) as u64;
    result.ts_min = Some(result.ts_min.map_or(ts, |m| m.min(ts)));
    result.ts_max = Some(result.ts_max.map_or(ts, |m| m.max(ts)));
    if result.prev_ts == Some(ts) {
        result.ts_repeats += 1;
    }
    result.prev_ts = Some(ts);
    let Some(frames) = block_frames(body, body_offset) else {
        return Ok(());
    };
    for (offset, start, len) in frames {
        let frame = &body[start..start + len];
        match codec {
            Codec::Av1 => scan_av1(frame, offset, result.frames, ts, collect, &mut result.items),
            Codec::Hevc => {
                result.el_units += scan_hevc(
                    frame,
                    offset,
                    result.frames,
                    len_size,
                    ts,
                    collect,
                    &mut result.items,
                )
            }
            Codec::Other => {}
        }
        result.frames += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(obu_type << 3) | 0x02];
        let mut len = payload.len();
        loop {
            let mut byte = (len & 0x7F) as u8;
            len >>= 7;
            if len != 0 {
                byte |= 0x80;
            }
            v.push(byte);
            if len == 0 {
                break;
            }
        }
        v.extend_from_slice(payload);
        v
    }

    /// With the extension header the payload sits one byte further on, and an
    /// in-place patch that missed it would write one byte early.
    #[test]
    fn payload_offsets_count_the_extension_byte() {
        let mut cll = vec![1u8];
        cll.extend_from_slice(&1768u16.to_be_bytes());
        cll.extend_from_slice(&505u16.to_be_bytes());
        cll.push(0x80);

        let mut frame = obu(2, &[]);
        let cll_at = frame.len();
        frame.push((5 << 3) | 0x06); // metadata, extension, size field
        frame.push(0x20); // temporal_id 1
        frame.push(cll.len() as u8);
        frame.extend_from_slice(&cll);
        frame.extend_from_slice(&[(6 << 3) | 0x06, 0x20, 2, 0, 0]); // frame, extension

        let mut items = Vec::new();
        scan_av1(&frame, 1000, 0, 0, false, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Cll);
        // header byte, extension byte, one length byte, the metadata_type byte
        assert_eq!(items[0].offset, 1000 + cll_at as u64 + 4);
        assert_eq!(items[0].len, 4);
    }

    #[test]
    fn finds_cll_and_mdcv_obus_and_points_at_their_payloads() {
        let mut cll = vec![1u8]; // metadata_type = 1
        cll.extend_from_slice(&1768u16.to_be_bytes());
        cll.extend_from_slice(&505u16.to_be_bytes());
        cll.push(0x80);
        let mut mdcv = vec![2u8];
        mdcv.extend_from_slice(&[0u8; 24]);
        mdcv.push(0x80);

        let mut frame = obu(2, &[0xAA, 0xBB]); // sequence header
        let cll_at = frame.len();
        frame.extend_from_slice(&obu(5, &cll));
        let mdcv_at = frame.len();
        frame.extend_from_slice(&obu(5, &mdcv));
        frame.extend_from_slice(&obu(6, &[0u8; 8])); // frame

        let mut items = Vec::new();
        scan_av1(&frame, 1000, 0, 0, false, &mut items);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::Cll);
        // header byte + one length byte + the metadata_type byte
        assert_eq!(items[0].offset, 1000 + cll_at as u64 + 3);
        assert_eq!(items[0].len, 4);
        assert_eq!(items[1].kind, ItemKind::Mdcv);
        assert_eq!(items[1].offset, 1000 + mdcv_at as u64 + 3);
        assert_eq!(items[1].len, 24);
        assert!(items.iter().all(|i| i.patchable_in_place));
    }

    #[test]
    fn recognises_dolby_vision_and_hdr10plus_by_their_provider_code() {
        let dv = obu(5, &[4, 0xB5, 0x00, 0x3B, 0x01, 0x02]);
        let hp = obu(5, &[4, 0xB5, 0x00, 0x3C, 0x00, 0x01]);
        let mut frame = dv;
        frame.extend_from_slice(&hp);
        let mut items = Vec::new();
        scan_av1(&frame, 0, 7, 0, false, &mut items);
        assert_eq!(items[0].kind, ItemKind::DolbyVisionRpu);
        assert_eq!(items[1].kind, ItemKind::Hdr10Plus);
        assert_eq!(items[0].frame, 7);
        // Dynamic metadata is not a fixed-size field, so it is never overwritten.
        assert!(items.iter().all(|i| !i.patchable_in_place));
    }

    #[test]
    fn hevc_sei_payloads_are_found_and_escaped_ones_are_flagged() {
        // content_light_level_info with values that need no escaping.
        let mut rbsp = vec![144u8, 4];
        rbsp.extend_from_slice(&1000u16.to_be_bytes());
        rbsp.extend_from_slice(&400u16.to_be_bytes());
        rbsp.push(0x80);
        let mut items = Vec::new();
        scan_hevc_sei(&rbsp, 0, 0, 0, false, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Cll);
        assert_eq!(items[0].offset, 2);
        assert!(items[0].patchable_in_place);

        // The same message carrying an emulation prevention byte must not be
        // overwritten in place: the payload on disk is longer than its value.
        let mut esc = vec![144u8, 5, 0x00, 0x00, 0x03, 0x01, 0xF4];
        esc.push(0x80);
        let mut items = Vec::new();
        scan_hevc_sei(&esc, 0, 0, 0, false, &mut items);
        assert_eq!(items.len(), 1);
        assert!(!items[0].patchable_in_place);
    }

    #[test]
    fn unlaced_blocks_yield_exactly_one_frame() {
        // track 1, timestamp 0, no lacing, then the payload.
        let body = vec![0x81, 0x00, 0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];
        let frames = block_frames(&body, 500).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], (504, 4, 4));
    }

    #[test]
    fn leb128_reads_multi_byte_values() {
        assert_eq!(leb128(&[0x7F], 0), Some((127, 1)));
        assert_eq!(leb128(&[0x80, 0x01], 0), Some((128, 2)));
    }
}
