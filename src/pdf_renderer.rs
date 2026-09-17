//! LaTeX → PDF renderer via tectonic.
//!
//! Takes a `.tex` string, writes it to a temp directory, invokes tectonic,
//! and returns the raw PDF bytes.
//!
//! The first call in a freshly built Docker image may take a few seconds while
//! tectonic reads its pre-cached package bundle.  Subsequent calls are fast.

use anyhow::{Context, Result};

/// Convert a LaTeX string to a PDF byte vector using tectonic.
///
/// tectonic must be on PATH.  Returns an error if the binary is not found
/// or exits non-zero.
pub async fn tex_to_pdf(tex: &str) -> Result<Vec<u8>> {
    let tmp = tempfile::tempdir().context("Failed to create temp dir")?;
    let tex_path = tmp.path().join("schedule.tex");
    let pdf_path = tmp.path().join("schedule.pdf");

    tokio::fs::write(&tex_path, tex.as_bytes()).await
        .context("Failed to write temp .tex file")?;

    let out = tokio::process::Command::new("tectonic")
        .args([
            "--outdir", tmp.path().to_str().unwrap(),
            "--print",
            tex_path.to_str().unwrap(),
        ])
        .output()
        .await
        .context("Failed to spawn tectonic — is it installed?")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = [stderr.trim(), stdout.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("tectonic exited {}: {}", out.status, detail);
    }

    tokio::fs::read(&pdf_path).await
        .context("tectonic succeeded but PDF output file not found")
}
