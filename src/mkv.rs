//! The Matroska head: everything before the first Cluster.
//!
//! This is the part of a file that holds the static HDR signalling, and it is a
//! few kilobytes no matter how large the file is. Reading it costs one seek.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::ebml::{self, Elem};

/// How much of the file we pull in to look for the first Cluster. Muxers put
/// Tracks well inside the first megabyte; attachments can push it further, so we
/// grow on demand up to `MAX_HEAD`.
const INITIAL_HEAD: usize = 1 << 20;
const MAX_HEAD: usize = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    Av1,
    Other,
}

impl Codec {
    pub fn from_codec_id(s: &str) -> Codec {
        match s {
            "V_MPEGH/ISO/HEVC" => Codec::Hevc,
            "V_AV1" => Codec::Av1,
            _ => Codec::Other,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
            Codec::Other => "other",
        }
    }
}

/// The video track, located inside the head.
#[derive(Debug, Clone)]
pub struct VideoTrack {
    pub track_entry: Elem,
    pub video: Elem,
    pub colour: Option<Elem>,
    pub codec_private: Option<Elem>,
    pub track_number: u64,
    pub codec_id: String,
    pub codec: Codec,
    pub pixel_width: u64,
    pub pixel_height: u64,
    /// Display dimensions when the container overrides the coded size.
    pub display_width: Option<u64>,
    pub display_height: Option<u64>,
    /// Nanoseconds per frame, when the track states it.
    pub default_duration_ns: Option<u64>,
}

/// What the timing report needs from every track, video included.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub number: u64,
    /// 1 video, 2 audio, 17 subtitles.
    pub track_type: u64,
    pub codec_id: String,
    pub default_duration_ns: Option<u64>,
    pub codec_delay_ns: Option<u64>,
    pub seek_pre_roll_ns: Option<u64>,
}

/// A parsed head, with the raw bytes kept so we can rebuild it byte for byte.
pub struct Head {
    pub buf: Vec<u8>,
    /// Absolute offset of the Segment body. Seek positions are relative to it.
    pub segment_body: usize,
    /// Absolute offset of the first Cluster, i.e. the end of the head.
    pub head_end: usize,
    /// Direct children of Segment that live before the first Cluster.
    pub top: Vec<Elem>,
    pub video: VideoTrack,
    /// Nanoseconds per timestamp tick, from Info. 1 ms unless the file says so.
    pub timestamp_scale: u64,
    /// Absolute offset of the Segment element itself.
    pub segment_offset: usize,
    /// Segment duration from Info, in ticks.
    pub duration_ticks: Option<f64>,
    pub muxing_app: String,
    pub writing_app: String,
    pub tracks: Vec<TrackInfo>,
}

fn read_prefix(path: &Path, len: usize) -> Result<Vec<u8>> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let file_len = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    let want = len.min(file_len as usize);
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

impl Head {
    pub fn read(path: &Path) -> Result<Head> {
        let mut want = INITIAL_HEAD;
        loop {
            let buf = read_prefix(path, want)?;
            match Head::parse(buf, want) {
                Ok(head) => return Ok(head),
                Err(ParseOutcome::NeedMore) if want < MAX_HEAD => {
                    want = (want * 4).min(MAX_HEAD);
                }
                Err(ParseOutcome::NeedMore) => {
                    bail!(
                        "no Cluster found in the first {} MiB - is this really a Matroska file?",
                        MAX_HEAD >> 20
                    )
                }
                Err(ParseOutcome::Fatal(e)) => return Err(e),
            }
        }
    }

    fn parse(buf: Vec<u8>, requested: usize) -> std::result::Result<Head, ParseOutcome> {
        let truncated = buf.len() == requested;
        let inner = || -> Result<Option<Head>> {
            let ebml_hdr = ebml::read_elem(&buf, 0)?;
            if ebml_hdr.id != ebml::ID_EBML {
                bail!("not a Matroska file: it does not start with an EBML header");
            }
            let seg = ebml::read_elem(&buf, ebml_hdr.body_end())?;
            if seg.id != ebml::ID_SEGMENT {
                bail!("not a Matroska file: no Segment after the EBML header");
            }
            let segment_body = seg.body;

            // Walk Segment children until the first Cluster.
            let mut top = Vec::new();
            let mut pos = segment_body;
            let mut head_end = None;
            while pos < buf.len() {
                let el = match ebml::read_elem(&buf, pos) {
                    Ok(e) => e,
                    Err(_) => return Ok(None), // header straddles the buffer edge
                };
                if el.id == ebml::ID_CLUSTER {
                    head_end = Some(el.offset);
                    break;
                }
                if el.unknown_size {
                    bail!(
                        "element {:#X} before the first Cluster has an unknown size; \
                         this file needs a full rewrite, not a head patch",
                        el.id
                    );
                }
                if el.body_end() > buf.len() {
                    return Ok(None); // body straddles the buffer edge
                }
                pos = el.body_end();
                top.push(el);
            }
            let Some(head_end) = head_end else {
                return Ok(None);
            };

            let tracks = ebml::find(&top, ebml::ID_TRACKS)
                .context("this file has no Tracks element")?;
            let video = find_video_track(&buf, tracks)?;

            let mut timestamp_scale = 1_000_000;
            let mut duration_ticks = None;
            let mut muxing_app = String::new();
            let mut writing_app = String::new();
            if let Some(info) = ebml::find(&top, ebml::ID_INFO) {
                let kids = ebml::children(&buf, info.body, info.body_end())?;
                if let Some(ts) = ebml::find(&kids, 0x2AD7B1) {
                    timestamp_scale = ebml::read_uint(&buf, ts)?;
                }
                if let Some(d) = ebml::find(&kids, 0x4489) {
                    duration_ticks = ebml::read_float(&buf, d).ok();
                }
                let text = |id: u32| -> String {
                    ebml::find(&kids, id)
                        .map(|e| String::from_utf8_lossy(&buf[e.body..e.body_end()]).into_owned())
                        .unwrap_or_default()
                };
                muxing_app = text(0x4D80);
                writing_app = text(0x5741);
            }
            let tracks = list_tracks(&buf, tracks)?;

            Ok(Some(Head {
                buf,
                segment_body,
                head_end,
                top,
                video,
                timestamp_scale,
                segment_offset: seg.offset,
                duration_ticks,
                muxing_app,
                writing_app,
                tracks,
            }))
        };

        match inner() {
            Ok(Some(h)) => Ok(h),
            Ok(None) if truncated => Err(ParseOutcome::NeedMore),
            Ok(None) => Err(ParseOutcome::Fatal(anyhow::anyhow!(
                "the file ends before the first Cluster"
            ))),
            Err(e) => Err(ParseOutcome::Fatal(e)),
        }
    }

    /// Segment-relative position, the way SeekHead stores it.
    pub fn seek_pos(&self, absolute: usize) -> u64 {
        (absolute - self.segment_body) as u64
    }
}

enum ParseOutcome {
    NeedMore,
    Fatal(anyhow::Error),
}

fn list_tracks(buf: &[u8], tracks: &Elem) -> Result<Vec<TrackInfo>> {
    let mut out = Vec::new();
    for entry in ebml::children(buf, tracks.body, tracks.body_end())?
        .iter()
        .filter(|e| e.id == ebml::ID_TRACK_ENTRY)
    {
        let kids = ebml::children(buf, entry.body, entry.body_end())?;
        let uint = |id: u32| -> Option<u64> {
            ebml::find(&kids, id).and_then(|e| ebml::read_uint(buf, e).ok())
        };
        out.push(TrackInfo {
            number: uint(ebml::ID_TRACK_NUMBER).unwrap_or(0),
            track_type: uint(ebml::ID_TRACK_TYPE).unwrap_or(0),
            codec_id: ebml::find(&kids, ebml::ID_CODEC_ID)
                .map(|e| String::from_utf8_lossy(&buf[e.body..e.body_end()]).into_owned())
                .unwrap_or_default(),
            default_duration_ns: uint(0x23E383),
            codec_delay_ns: uint(0x56AA),
            seek_pre_roll_ns: uint(0x56BB),
        });
    }
    Ok(out)
}

fn find_video_track(buf: &[u8], tracks: &Elem) -> Result<VideoTrack> {
    let entries = ebml::children(buf, tracks.body, tracks.body_end())?;
    for entry in entries.iter().filter(|e| e.id == ebml::ID_TRACK_ENTRY) {
        let kids = ebml::children(buf, entry.body, entry.body_end())?;
        let track_type = ebml::find(&kids, ebml::ID_TRACK_TYPE)
            .map(|e| ebml::read_uint(buf, e))
            .transpose()?
            .unwrap_or(0);
        if track_type != 1 {
            continue; // 1 = video
        }
        let video = *ebml::find(&kids, ebml::ID_VIDEO)
            .context("the video track has no Video element")?;
        let vkids = ebml::children(buf, video.body, video.body_end())?;
        let codec_id = ebml::find(&kids, ebml::ID_CODEC_ID)
            .map(|e| String::from_utf8_lossy(&buf[e.body..e.body_end()]).to_string())
            .unwrap_or_default();
        let uint = |list: &[Elem], id: u32| -> Result<Option<u64>> {
            match ebml::find(list, id) {
                Some(e) => Ok(Some(ebml::read_uint(buf, e)?)),
                None => Ok(None),
            }
        };
        return Ok(VideoTrack {
            track_entry: *entry,
            video,
            colour: ebml::find(&vkids, ebml::ID_COLOUR).copied(),
            codec_private: ebml::find(&kids, ebml::ID_CODEC_PRIVATE).copied(),
            track_number: uint(&kids, ebml::ID_TRACK_NUMBER)?.unwrap_or(1),
            codec: Codec::from_codec_id(&codec_id),
            codec_id,
            pixel_width: uint(&vkids, ebml::ID_PIXEL_WIDTH)?.unwrap_or(0),
            pixel_height: uint(&vkids, ebml::ID_PIXEL_HEIGHT)?.unwrap_or(0),
            display_width: uint(&vkids, 0x54B0)?,
            display_height: uint(&vkids, 0x54BA)?,
            default_duration_ns: uint(&kids, 0x23E383)?,
        });
    }
    bail!("this file has no video track")
}
