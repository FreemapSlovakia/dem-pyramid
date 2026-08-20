//! Thin wrappers over the GDAL command line tools.
//!
//! Everything this tool needs from GDAL is available through gdalinfo,
//! gdalsrsinfo and ogr2ogr, so it never links libgdal. That keeps builds fast
//! and decouples the tool from whichever GDAL the host happens to have.

use anyhow::{Context, Result, bail};
use std::process::Command;

pub fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;

    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_quiet(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

pub fn info_json(path: &str) -> Result<serde_json::Value> {
    let text = run("gdalinfo", &["-json", path])?;
    Ok(serde_json::from_str(&text)?)
}

/// `EPSG:1234` for anything GDAL can identify, else None.
fn srs_epsg(target: &str) -> Option<String> {
    let s = run_quiet("gdalsrsinfo", &["-o", "epsg", "--single-line", target])?;
    let line = s.lines().find(|l| l.starts_with("EPSG:"))?;
    Some(line.trim().to_owned())
}

/// PROJ.4 string with irrelevant tokens dropped and the rest sorted, so two
/// spellings of the same CRS compare equal.
fn srs_proj4_normalised(target: &str) -> Option<String> {
    let s = run_quiet("gdalsrsinfo", &["-o", "proj4", "--single-line", target])?;
    let mut parts: Vec<&str> = s
        .trim()
        .trim_matches('\'')
        .split_whitespace()
        .filter(|t| !matches!(*t, "+no_defs" | "+type=crs" | "+wktext"))
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort_unstable();
    Some(parts.join(" "))
}

pub fn srs_wkt(target: &str) -> Option<String> {
    run_quiet("gdalsrsinfo", &["-o", "wkt1", "--single-line", target])
}

/// Compare a dataset's CRS against a declared one the way a human would.
///
/// A plain WKT comparison is too strict: it separates a compound CRS from its
/// horizontal part (SWEREF99 TM + RH2000 vs EPSG:5845) and trips over axis
/// order. Compare authority codes first, then fall back to normalised PROJ.4,
/// which is what saves sources whose WKT carries no authority at all.
pub fn crs_matches(dataset: &str, declared: &str) -> bool {
    if let (Some(a), Some(b)) = (srs_epsg(dataset), srs_epsg(declared))
        && a == b
    {
        return true;
    }
    match (
        srs_proj4_normalised(dataset),
        srs_proj4_normalised(declared),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}
