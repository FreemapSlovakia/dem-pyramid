//! AVIF encoding, through libavif's command line encoder.
//!
//! Shelling out rather than linking, for the same reason `gdal_cli` does: the
//! host's libavif already has aom, rav1e, svt-av1 and dav1d wired in, while
//! `libavif-sys` offers no way to link a system libavif and instead vendors
//! the C library and builds a codec from source.
//!
//! Why AVIF at all: this renderer draws smooth gradients, which PNG compressed
//! beautifully until the sky dither put noise on every pixel and took a
//! 7200x600 render past 4 MB. AVIF carries the same picture, dither included,
//! in under 300 KB.
//!
//! Why not something cheaper: most lossy settings throw the dither away and
//! quantise the gradient straight back into bands. JPEG at q85 is small and
//! useless -- 91 of 100 sky rows flat, with steps of three levels, worse than
//! the undithered PNG it would replace. Lossless AVIF is 2.5 MB, worse than
//! lossless WebP. Only AVIF above about q93 is both small and faithful, which
//! is measurable: every sky row still varies. See `QUALITY` for where the
//! threshold sits and why the default leaves margin above it.

use anyhow::{Context, Result, bail};
use image::ImageEncoder;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Paired with 10-bit encoding, 95 puts the worst step in a soft sky at about
/// a quarter of a level -- under what 8-bit output can show. At 8 bits the
/// same setting leaves steps of a full level, and no quality setting fixes
/// that: q99 is larger and still visibly worse.
///
/// Below about 93 the encoder also starts smoothing the sky dither away
/// outright, and the banding it exists to prevent comes back. That threshold
/// moves with the picture, since rate allocation gives a smooth frame fewer
/// bits, so this leaves margin rather than sitting on the edge.
pub const QUALITY: u8 = 95;
/// 1..10, coarser being faster. 6 costs about a quarter second on a render
/// that takes twenty, and buys a third off the file against 10.
pub const SPEED: u8 = 6;

/// Deletes its paths on the way out, however the function returns.
struct Scratch(Vec<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A directory only this process can write to, created once, mode 0700.
///
/// The intermediates cannot live loose in a shared /tmp. Their names would be
/// guessable -- pid plus a counter that restarts at zero -- and both
/// `File::create` and avifenc's `fopen` follow symlinks, so any local user
/// could plant a symlink at the next name and have the service truncate a file
/// on its behalf, or plant a directory there and turn every render into a 500.
/// Creating the directory with `mkdir(0700)` is atomic, so it cannot be
/// pre-empted the way an open can.
fn scratch_dir() -> Result<&'static Path> {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(d) = DIR.get() {
        return Ok(d);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let path = std::env::temp_dir().join(format!(
        "dem-panorama-{}-{nanos:09}",
        std::process::id()
    ));
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(DIR.get_or_init(|| path))
}

/// Whether the encoder is actually installed.
///
/// AVIF is the default format, so without it every panorama request fails.
/// Better to say so once at startup than to discover it one 500 at a time.
pub fn available() -> bool {
    Command::new("avifenc")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn encode(img: &image::RgbImage, quality: u8, speed: u8) -> Result<Vec<u8>> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = scratch_dir()?;
    let stamp = SEQ.fetch_add(1, Ordering::Relaxed);
    let src = dir.join(format!("{stamp}.png"));
    let dst = dir.join(format!("{stamp}.avif"));
    let _scratch = Scratch(vec![src.clone(), dst.clone()]);

    write_fast_png(&src, img)?;

    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let out = Command::new("avifenc")
        .args([
            "-q",
            &quality.to_string(),
            "-s",
            &speed.to_string(),
            // No chroma subsampling: the silhouettes are one pixel wide by
            // default and are the point of the picture.
            "-y",
            "444",
            // Encode at 10 bits even though the picture is 8 and the display
            // almost certainly is too. The headroom is for the *encoder*: its
            // quantisation error then lands below what 8-bit output can show,
            // and the browser rounds back down on decode. It beats spending
            // the same bytes on quality -- q99 at 8 bits is larger than q95 at
            // 10 and still leaves steps of a full level in a soft sky.
            "-d",
            "10",
            "--jobs",
            &threads.to_string(),
            src.to_str().context("non-utf8 temp path")?,
            dst.to_str().context("non-utf8 temp path")?,
        ])
        .output()
        .context("failed to run avifenc -- is libavif-bin installed?")?;

    if !out.status.success() {
        bail!(
            "avifenc failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::read(&dst).with_context(|| format!("reading {}", dst.display()))
}

/// The intermediate only has to survive a few milliseconds and one read, so it
/// is written with compression off. Left at the default it would spend longer
/// packing a file we are about to delete than avifenc spends on the real one.
///
/// Encoded into memory and written in one call, rather than streamed into the
/// file. `write_image` consumes its writer, so the closing chunk is emitted
/// from the encoder's `Drop` -- which cannot report failure. Streaming into a
/// full disk would leave a truncated PNG and return `Ok`, and the request
/// would fail later as an unexplained avifenc decode error rather than as the
/// write error it is.
fn write_fast_png(path: &Path, img: &image::RgbImage) -> Result<()> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut buf,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    std::fs::write(path, &buf).with_context(|| format!("writing {}", path.display()))
}
