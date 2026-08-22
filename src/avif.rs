//! AVIF encoding, through libavif's command line encoder.
//!
//! Shelling out rather than linking, for the same reason `gdal_cli` does: the
//! host's libavif already has aom, rav1e, svt-av1 and dav1d wired in, while
//! `libavif-sys` offers no way to link a system libavif and instead vendors
//! the C library and builds a codec from source.
//!
//! Why AVIF at all: this renderer draws smooth gradients, which PNG compressed
//! beautifully until the sky dither put a level of noise on every pixel and
//! took a 7200x600 render from about 600 KB to 3.7 MB. AVIF carries the same
//! picture, dither included, in 119 KB.
//!
//! Why not something cheaper: most lossy settings throw the dither away and
//! quantise the gradient straight back into bands. JPEG at q85 is small and
//! useless -- 91 of 100 sky rows flat, with steps of three levels, worse than
//! the undithered PNG it would replace. Lossless AVIF is 2.5 MB, worse than
//! lossless WebP. Only AVIF around q90 is both small and faithful, which is
//! measurable: every sky row still varies.

use anyhow::{Context, Result, bail};
use image::ImageEncoder;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Below about 93 the encoder starts smoothing the sky dither away and the
/// banding it exists to prevent comes back -- at q90 on a supersampled render,
/// 42 of 100 sky rows collapse to a single flat value. The threshold moves
/// with the picture, because rate allocation gives a smooth frame fewer bits,
/// so 95 leaves margin rather than sitting on the edge.
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

pub fn encode(img: &image::RgbImage, quality: u8, speed: u8) -> Result<Vec<u8>> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let stamp = format!(
        "dem-panorama-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dir = std::env::temp_dir();
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
fn write_fast_png(path: &Path, img: &image::RgbImage) -> Result<()> {
    let file = std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
    );
    image::codecs::png::PngEncoder::new_with_quality(
        file,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(())
}
