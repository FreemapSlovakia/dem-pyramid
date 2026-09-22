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

pub fn srs_wkt(target: &str) -> Option<String> {
    run_quiet("gdalsrsinfo", &["-o", "wkt1", "--single-line", target])
}
