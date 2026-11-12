//! Motion backdrops: Apple serves them as HLS, so a variant is picked from the
//! master playlist and written as a plain MP4 with ffmpeg. Apple's renditions
//! are encoded at a generous bitrate, so SDR ones are re-encoded with x264
//! (about half the size, visually the same); otherwise the stream is copied.
//! `gatefold optimize` gives backdrops already on disk the same re-encode
//! ([`shrink`]).

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Backdrops above this are a lot of bytes for no visible gain.
pub const MAX_HEIGHT: u32 = 1080;

/// True when `ffmpeg` is on `PATH`; without it motion backdrops are skipped.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// One `#EXT-X-STREAM-INF` entry of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub height: u32,
    pub codecs: String,
    pub sdr: bool,
}

impl Variant {
    fn h264(&self) -> bool {
        self.codecs.starts_with("avc1")
    }
}

/// Parse the variants of a master playlist, resolving relative URIs against the
/// playlist's own URL.
pub fn variants(master: &str, master_url: &str) -> Vec<Variant> {
    let base = master_url.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut found = Vec::new();
    let mut lines = master.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let Some(uri) = lines.find(|l| !l.is_empty() && !l.starts_with('#')) else {
            break;
        };
        let attrs = attributes(attrs);
        let get = |key: &str| {
            attrs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
        };
        let Some(bandwidth) = get("BANDWIDTH").and_then(|v| v.parse().ok()) else {
            continue;
        };
        let height = get("RESOLUTION")
            .and_then(|r| r.split_once('x'))
            .and_then(|(_, h)| h.parse().ok())
            .unwrap_or(0);
        let url = if uri.contains("://") {
            uri.to_string()
        } else {
            format!("{base}/{uri}")
        };
        found.push(Variant {
            url,
            bandwidth,
            height,
            codecs: get("CODECS").unwrap_or("").to_string(),
            sdr: get("VIDEO-RANGE").is_none_or(|r| r == "SDR"),
        });
    }
    found
}

/// Split `KEY=VALUE,KEY="quoted,value"` attribute lists.
fn attributes(list: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let mut rest = list;
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            let remainder = quoted[end..].strip_prefix('"').unwrap_or("");
            (quoted[..end].to_string(), remainder)
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        out.push((key.trim(), value));
        rest = remainder.strip_prefix(',').unwrap_or(remainder);
    }
    out
}

/// The variant to keep: the highest bandwidth at or under [`MAX_HEIGHT`],
/// preferring SDR H.264 (plays everywhere) over HDR/HEVC renditions.
pub fn pick(variants: &[Variant]) -> Option<&Variant> {
    let fitting: Vec<&Variant> = variants.iter().filter(|v| v.height <= MAX_HEIGHT).collect();
    let pool: Vec<&Variant> = if fitting.is_empty() {
        variants.iter().collect()
    } else {
        fitting
    };
    let tiers: [&dyn Fn(&Variant) -> bool; 3] = [&|v| v.sdr && v.h264(), &|v| v.sdr, &|_| true];
    tiers.iter().find_map(|tier| {
        pool.iter()
            .copied()
            .filter(|v| tier(v))
            .max_by_key(|v| v.bandwidth)
    })
}

/// x264 constant-quality factor (ffmpeg's default): no visible loss on
/// backdrops at roughly half Apple's bitrate.
const CRF: &str = "23";

/// True when this ffmpeg build has the `libx264` encoder (checked once).
fn libx264_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("libx264"))
    })
}

/// Downscale to fit 1920x1080 (keeping the aspect ratio, never upscaling), for
/// artists whose playlist has no rendition at or under [`MAX_HEIGHT`].
const FIT_1080P: &str =
    "scale='min(1920,iw)':'min(1080,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2";

/// The re-encode every SDR motion backdrop gets: x264, fitted into 1080p,
/// without audio (backdrops play muted).
const SHRINK_ARGS: &[&str] = &[
    "-an", "-vf", FIT_1080P, "-c:v", "libx264", "-preset", "medium", "-crf", CRF, "-pix_fmt",
    "yuv420p",
];

/// Whether `variant` gets [`shrink`]: SDR renditions do when x264 is there;
/// HDR ones are stream-copied, since squeezing them into 8-bit SDR without
/// tone mapping would wash out the colours.
fn reencodes(variant: &Variant, x264: bool) -> bool {
    x264 && variant.sdr
}

/// Write one variant playlist into `out` as MP4 (`faststart`): [`shrink`]ed
/// when it can be (see [`reencodes`]), else stream-copied.
pub fn remux(variant: &Variant, user_agent: &str, out: &Path) -> Result<()> {
    let input = OsStr::new(&variant.url);
    if reencodes(variant, libx264_available()) {
        shrink(input, Some(user_agent), out)
    } else {
        ffmpeg(input, Some(user_agent), &["-c", "copy"], out)
    }
}

/// Re-encode the SDR motion backdrop `input` (a playlist URL, or a file
/// already on disk) into `out` as MP4: [`SHRINK_ARGS`], never a copy. Every
/// downloaded backdrop gets this, and `gatefold optimize` gives it to older
/// ones. Fails when this ffmpeg has no `libx264`.
pub fn shrink(input: &OsStr, user_agent: Option<&str>, out: &Path) -> Result<()> {
    if !libx264_available() {
        bail!("this ffmpeg has no libx264 encoder");
    }
    ffmpeg(input, user_agent, SHRINK_ARGS, out)
}

/// True when this ffmpeg can [`shrink`].
pub fn can_shrink() -> bool {
    libx264_available()
}

/// A motion backdrop already on disk, as `gatefold optimize` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Apple's rendition as downloaded: worth [`shrink`]ing.
    Original,
    /// Already re-encoded with x264 (by gatefold): another pass would only
    /// lose quality.
    Shrunk,
    /// HDR, which downloads keep as they are too.
    Hdr,
}

/// x264 writes its version and settings into every stream it encodes ("x264 -
/// core 164 …"), near the start; Apple's encoder doesn't.
const X264_SIGNATURE: &[u8] = b"x264 - core ";
/// With `faststart` the index comes first; the first frame follows within this.
const SIGNATURE_WINDOW: u64 = 16 * 1024 * 1024;

pub fn inspect(path: &Path) -> Result<Existing> {
    let mut head = Vec::new();
    File::open(path)
        .and_then(|file| file.take(SIGNATURE_WINDOW).read_to_end(&mut head))
        .with_context(|| format!("reading {}", path.display()))?;
    if head
        .windows(X264_SIGNATURE.len())
        .any(|w| w == X264_SIGNATURE)
    {
        return Ok(Existing::Shrunk);
    }
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=pix_fmt,color_transfer",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running ffprobe")?;
    if !output.status.success() {
        bail!("ffprobe couldn't read {}", path.display());
    }
    Ok(if is_hdr(&String::from_utf8_lossy(&output.stdout)) {
        Existing::Hdr
    } else {
        Existing::Original
    })
}

/// From ffprobe's `pix_fmt=…` / `color_transfer=…` lines: a PQ (HDR10, Dolby
/// Vision) or HLG transfer, or, when the file doesn't say, more than 8 bits
/// per sample (10-bit video is HDR in practice, and squeezing it into 8-bit
/// SDR would band and wash out). Downloads keep these as they are.
fn is_hdr(probe: &str) -> bool {
    probe.lines().any(|line| match line.trim().split_once('=') {
        Some(("color_transfer", transfer)) => matches!(transfer, "smpte2084" | "arib-std-b67"),
        Some(("pix_fmt", format)) => ["p10", "p12", "p16", "p010", "p016"]
            .iter()
            .any(|deep| format.contains(deep)),
        _ => false,
    })
}

/// Run ffmpeg from `input` into `out` as MP4 (`faststart`) with `codec` args.
fn ffmpeg(input: &OsStr, user_agent: Option<&str>, codec: &[&str], out: &Path) -> Result<()> {
    let mut command = Command::new("ffmpeg");
    command.args(["-loglevel", "error", "-nostdin"]);
    if let Some(user_agent) = user_agent {
        command.args(["-user_agent", user_agent]);
    }
    let output = command
        .arg("-i")
        .arg(input)
        .args(codec)
        .args(["-movflags", "+faststart", "-f", "mp4", "-y"])
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running ffmpeg")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().last().unwrap_or("").trim();
        bail!("ffmpeg failed ({}): {detail}", output.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=12646304,VIDEO-RANGE=SDR,CODECS="avc1.640028",RESOLUTION=1920x1080
rel/1080.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=30000000,VIDEO-RANGE=PQ,CODECS="hvc1.2.4.L153.B0",RESOLUTION=3840x2160
https://cdn.example/2160.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=20000000,VIDEO-RANGE=SDR,CODECS="hvc1.2.20000000.L123.B0",RESOLUTION=1920x1080
rel/1080-hevc.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3341692,CODECS="avc1.64001f,mp4a.40.2",RESOLUTION=1280x720
rel/720.m3u8
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI="iframes.m3u8"
"#;

    #[test]
    fn parses_and_picks_the_best_fitting_h264_variant() {
        let found = variants(MASTER, "https://host/path/master.m3u8");
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].url, "https://host/path/rel/1080.m3u8");
        assert_eq!(found[1].url, "https://cdn.example/2160.m3u8");
        assert!(!found[1].sdr);
        assert_eq!(found[3].codecs, "avc1.64001f,mp4a.40.2");
        assert!(found[3].sdr);
        assert_eq!(
            pick(&found).map(|v| v.url.as_str()),
            Some("https://host/path/rel/1080.m3u8")
        );
    }

    #[test]
    fn falls_back_when_nothing_fits() {
        let found = variants(MASTER, "https://host/master.m3u8");
        let only_big: Vec<Variant> = found.into_iter().filter(|v| v.height > 1080).collect();
        assert_eq!(pick(&only_big).map(|v| v.height), Some(2160));
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn reencodes_only_sdr_variants_when_x264_is_there() {
        let found = variants(MASTER, "https://host/master.m3u8");
        let (sdr, hdr) = (&found[0], &found[1]);
        assert!(reencodes(sdr, true));
        assert!(!reencodes(sdr, false));
        assert!(!reencodes(hdr, true));
        for arg in ["libx264", "-an", FIT_1080P] {
            assert!(SHRINK_ARGS.contains(&arg), "{arg}");
        }
    }

    #[test]
    fn tells_hdr_apart() {
        assert!(is_hdr("pix_fmt=yuv420p10le\ncolor_transfer=smpte2084\n"));
        assert!(is_hdr("pix_fmt=yuv420p\ncolor_transfer=arib-std-b67\n"));
        // Untagged, but 10-bit.
        assert!(is_hdr("pix_fmt=yuv420p10le\ncolor_transfer=unknown\n"));
        assert!(is_hdr("pix_fmt=p010le\n"));
        assert!(!is_hdr("pix_fmt=yuv420p\ncolor_transfer=bt709\n"));
        assert!(!is_hdr("pix_fmt=yuv420p\ncolor_transfer=unknown\n"));
        assert!(!is_hdr(""));
    }

    #[test]
    fn splits_quoted_attributes() {
        let attrs = attributes(r#"A=1,B="x,y",C=z"#);
        assert_eq!(
            attrs,
            vec![
                ("A", "1".to_string()),
                ("B", "x,y".to_string()),
                ("C", "z".to_string())
            ]
        );
    }
}
