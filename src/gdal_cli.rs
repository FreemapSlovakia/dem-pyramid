//! Thin wrappers over the GDAL command line tools.
//!
//! For what is simpler read off a tool's output than through the bindings:
//! gdalinfo's JSON, and ogr2ogr's CSV.

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

pub fn info_json(path: &str) -> Result<serde_json::Value> {
    let text = run("gdalinfo", &["-json", path])?;
    Ok(serde_json::from_str(&text)?)
}
