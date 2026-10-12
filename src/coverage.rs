//! Where each dataset actually has data, coarsely, for crediting.
//!
//! A raster's own extent says little: terrain-tiles' rasters are aligned to the
//! XYZ tile grid, so their boxes reach hundreds of kilometres past the country
//! they hold, and a render crediting by box names half the continent. So each
//! dataset's coarsest index row is read once at startup into a bit per cell of
//! a z7 pixel, about 800 m at 49N, set where it has data.

use anyhow::{Context, Result};
use gdal::Dataset;
use gdal::raster::ResampleAlg;
use rayon::prelude::*;
use std::sync::Arc;

use crate::config::{Doc, level_res};
use crate::grid::merc_to_lonlat;

/// Cells are z7 pixels whatever level the row was read from: fine enough that
/// no dataset in the catalogue falls between two of them, coarse enough that
/// the largest -- GEDTM30 over Europe -- is 40 M bits.
const CELL_LEVEL: u32 = 7;

pub struct Coverage {
    /// Web Mercator top-left corner and cell size per axis.
    x0: f64,
    y0: f64,
    cell_x: f64,
    cell_y: f64,
    w: usize,
    h: usize,
    bits: Vec<u64>,
}

impl std::fmt::Debug for Coverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Coverage({}x{} cells of {:.0} m)",
            self.w, self.h, self.cell_x
        )
    }
}

impl Coverage {
    /// Read from a level's `vrt://` row, along with the lon/lat box of the
    /// cells that hold data, or `None` for that where none do.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn read(location: &str) -> Result<(Self, Option<[f64; 4]>)> {
        let ds = Dataset::open(location).with_context(|| format!("opening {location}"))?;
        let gt = ds.geo_transform()?;
        let (wid, hgt) = ds.raster_size();
        let factor = ((level_res(CELL_LEVEL) / gt[1]).round() as usize).max(1);
        let (w, h) = (wid.div_ceil(factor), hgt.div_ceil(factor));

        // Average skips nodata, so a cell is data wherever any of its pixels
        // is -- a coastline or a border strip keeps its cells.
        let buf = ds.rasterband(1)?.read_as::<u16>(
            (0, 0),
            (wid, hgt),
            (w, h),
            Some(ResampleAlg::Average),
        )?;

        let mut bits = vec![0u64; (w * h).div_ceil(64)];
        let (mut c0, mut r0, mut c1, mut r1) = (usize::MAX, usize::MAX, 0, 0);

        // 0 is the index's nodata.
        for (i, &v) in buf.data().iter().enumerate() {
            if v != 0 {
                bits[i / 64] |= 1 << (i % 64);
                let (r, c) = (i / w, i % w);
                (c0, r0, c1, r1) = (c0.min(c), r0.min(r), c1.max(c), r1.max(r));
            }
        }

        let (cell_x, cell_y) = (
            gt[1] * wid as f64 / w as f64,
            -gt[5] * hgt as f64 / h as f64,
        );
        let coverage = Self {
            x0: gt[0],
            y0: gt[3],
            cell_x,
            cell_y,
            w,
            h,
            bits,
        };

        let bbox = (c0 != usize::MAX).then(|| {
            let (west, north) =
                merc_to_lonlat(gt[0] + c0 as f64 * cell_x, gt[3] - r0 as f64 * cell_y);
            let (east, south) = merc_to_lonlat(
                gt[0] + (c1 + 1) as f64 * cell_x,
                gt[3] - (r1 + 1) as f64 * cell_y,
            );
            [west, south, east, north]
        });

        Ok((coverage, bbox))
    }

    /// Whether the cell holding a Web Mercator point has data.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn has(&self, x: f64, y: f64) -> bool {
        let (c, r) = ((x - self.x0) / self.cell_x, (self.y0 - y) / self.cell_y);

        if c < 0.0 || r < 0.0 || c >= self.w as f64 || r >= self.h as f64 {
            return false;
        }

        let i = r as usize * self.w + c as usize;
        self.bits[i / 64] & (1 << (i % 64)) != 0
    }
}

/// Every source's coverage, read in parallel, its box narrowed to where it
/// has data -- which also sizes the crediting sweep to the smallest dataset
/// rather than to the smallest padded raster.
///
/// One that cannot be read is left without, and credited by its raster's
/// box rather than keeping the server from starting: crediting it too widely
/// is the lesser fault.
pub fn attach(doc: &mut Doc) {
    let read: Vec<Result<(Coverage, Option<[f64; 4]>)>> = doc
        .sources
        .par_iter()
        .map(|s| Coverage::read(&s.location))
        .collect();

    for (source, read) in doc.sources.iter_mut().zip(read) {
        match read {
            Ok((coverage, bbox)) => {
                if let Some(bbox) = bbox {
                    source.bbox = bbox;
                }
                source.coverage = Some(Arc::new(coverage));
            }
            Err(e) => eprintln!("{}: no coverage, credited by its box: {e:#}", source.id),
        }
    }
}

#[cfg(test)]
pub(crate) fn from_cells(x0: f64, y0: f64, cell: f64, rows: &[&str]) -> Coverage {
    let (h, w) = (rows.len(), rows[0].len());
    let mut bits = vec![0u64; (w * h).div_ceil(64)];
    for (r, row) in rows.iter().enumerate() {
        for (c, ch) in row.chars().enumerate() {
            if ch == '#' {
                let i = r * w + c;
                bits[i / 64] |= 1 << (i % 64);
            }
        }
    }
    Coverage {
        x0,
        y0,
        cell_x: cell,
        cell_y: cell,
        w,
        h,
        bits,
    }
}
