//! Pull the metadata out of a file in one read.
//!
//! Everything a file knows about its grade comes off it in a single pass over
//! the Clusters: the Dolby Vision RPUs, the HDR10+ units, the static values,
//! the geometry. `extract` writes the dynamic part to disk in the formats
//! dovi_tool and hdr10plus_tool use; `rewrite --from` takes the same
//! collection straight into another file without touching the disk.
//!
//! The lists are in presentation order, which is what those tools write and
//! what the writer reads - and which for HEVC is not the order the frames sit
//! in the file.

use anyhow::{bail, Context, Result};
use std::io::{BufWriter, Write};
use std::path::Path;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use hdr10plus::metadata::Hdr10PlusMetadata;
use hdr10plus::metadata_json::{Hdr10PlusJsonMetadata, MetadataJsonRoot};

use crate::colour::{self, Mdcv};
use crate::mkv::{Codec, Head};
use crate::scan::{self, ItemKind};
use crate::stream::{self, Cll};

/// What one read of a file yields.
pub struct Collected {
    pub codec: Codec,
    pub frames: u64,
    /// True when the frames were not in presentation order on disk.
    pub reordered: bool,
    /// Coded picture size from the container.
    pub width: u64,
    pub height: u64,
    /// One per frame that carries one, in presentation order.
    pub rpus: Vec<DoviRpu>,
    pub hdr10plus: Vec<Hdr10PlusMetadata>,
    /// The static values, from the stream first and the container as fallback.
    pub cll: Option<Cll>,
    pub mdcv: Option<Mdcv>,
}

/// Read a file once for everything it carries, reporting bytes read against
/// bytes to read as it goes.
pub fn collect_progress(path: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<Collected> {
    let head = Head::read(path)?;
    let codec = head.video.codec;
    if codec == Codec::Other {
        bail!("extraction handles AV1 and HEVC; this track is {}", head.video.codec_id);
    }
    let found = scan::scan_progress(path, &head, u64::MAX, true, progress)?;

    // Presentation order: sort by timestamp, keeping file order for ties.
    let mut order: Vec<usize> = (0..found.items.len()).collect();
    order.sort_by_key(|&i| found.items[i].ts);
    let reordered = order.windows(2).any(|w| w[0] > w[1]);

    let mut rpus = Vec::new();
    let mut hdr10plus = Vec::new();
    for &i in &order {
        let item = &found.items[i];
        match item.kind {
            ItemKind::DolbyVisionRpu => {
                let bytes = item
                    .payload
                    .as_ref()
                    .context("RPU payload was not collected")?;
                let rpu = match codec {
                    Codec::Av1 => DoviRpu::parse_itu_t35_dovi_metadata_obu(bytes),
                    _ => DoviRpu::parse_unspec62_nalu(bytes),
                }
                .with_context(|| format!("parsing the RPU of frame {}", item.frame))?;
                rpus.push(rpu);
            }
            ItemKind::Hdr10Plus => {
                let bytes = item
                    .payload
                    .as_ref()
                    .context("HDR10+ payload was not collected")?;
                hdr10plus.push(Hdr10PlusMetadata::parse(bytes).with_context(|| {
                    format!("parsing the HDR10+ metadata of frame {}", item.frame)
                })?);
            }
            _ => {}
        }
    }

    // The stream wins: it is what a decoder reads.
    let container = colour::read(&head.buf, head.video.colour.as_ref())?;
    let mut cll = match found.first(ItemKind::Cll) {
        Some(item) => Some(stream::decode_cll(&item.payload_bytes(path)?)?),
        None => None,
    };
    if cll.is_none() {
        let a = container.max_cll.unwrap_or(0);
        let b = container.max_fall.unwrap_or(0);
        if a != 0 || b != 0 {
            cll = Some(Cll {
                max_cll: a.min(65535) as u16,
                max_fall: b.min(65535) as u16,
            });
        }
    }
    let mdcv = match found.first(ItemKind::Mdcv) {
        Some(item) => Some(stream::decode_mdcv(&item.payload_bytes(path)?, codec)?),
        None => container.mdcv,
    };

    Ok(Collected {
        codec,
        frames: found.frames,
        reordered,
        width: head.video.pixel_width,
        height: head.video.pixel_height,
        rpus,
        hdr10plus,
        cll,
        mdcv,
    })
}

/// The RPU list in dovi_tool's file format: a four byte start code, then
/// the NAL payload without its two header bytes.
pub fn write_rpu_file(rpus: &[DoviRpu], out: &Path) -> Result<()> {
    let mut file = BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
    for rpu in rpus {
        let nal = rpu.write_hevc_unspec62_nalu()?;
        file.write_all(&[0, 0, 0, 1])?;
        file.write_all(&nal[2..])?;
    }
    file.flush()?;
    Ok(())
}

/// The HDR10+ list as hdr10plus_tool's JSON document.
pub fn hdr10plus_json(list: &[Hdr10PlusMetadata]) -> serde_json::Value {
    let refs: Vec<&Hdr10PlusMetadata> = list.iter().collect();
    hdr10plus::metadata_json::generate_json(&refs, "ddvt_patch", env!("CARGO_PKG_VERSION"))
}

/// The HDR10+ list in the shape the encoder takes - the same path the JSON
/// file goes through, minus the file.
pub fn hdr10plus_entries(list: &[Hdr10PlusMetadata]) -> Result<Vec<Hdr10PlusJsonMetadata>> {
    let root: MetadataJsonRoot = serde_json::from_value(hdr10plus_json(list))
        .context("re-reading the generated HDR10+ JSON")?;
    Ok(root.scene_info)
}

#[derive(Debug, Default)]
pub struct ExtractStats {
    pub frames: u64,
    pub rpus: usize,
    pub hdr10plus: usize,
    pub reordered: bool,
}

pub fn run(
    path: &Path,
    rpu_out: Option<&Path>,
    hdr10plus_out: Option<&Path>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<ExtractStats> {
    for out in [rpu_out, hdr10plus_out].into_iter().flatten() {
        if crate::samepath::same_file(path, out) {
            bail!(
                "{} is the file being read; writing the metadata there would destroy it",
                out.display()
            );
        }
    }
    let c = collect_progress(path, progress)?;
    let stats = ExtractStats {
        frames: c.frames,
        rpus: c.rpus.len(),
        hdr10plus: c.hdr10plus.len(),
        reordered: c.reordered,
    };
    if let Some(out) = rpu_out {
        if c.rpus.is_empty() {
            bail!("the stream carries no Dolby Vision RPU");
        }
        write_rpu_file(&c.rpus, out)?;
    }
    if let Some(out) = hdr10plus_out {
        if c.hdr10plus.is_empty() {
            bail!("the stream carries no HDR10+ metadata");
        }
        let json = hdr10plus_json(&c.hdr10plus);
        let mut file = BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
        serde_json::to_writer_pretty(&mut file, &json)?;
        file.flush()?;
    }
    Ok(stats)
}
