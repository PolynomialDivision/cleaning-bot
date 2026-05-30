//! Headless Chromium → PDF renderer.
//!
//! Takes an HTML string, writes it to a temp file, invokes Chromium with
//! `--print-to-pdf`, and returns the raw PDF bytes.
//!
//! Docker-safe flags used: --no-sandbox, --disable-dev-shm-usage, --disable-gpu.

use anyhow::{Context, Result};

const CHROME_CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
];

/// Convert an HTML string to a PDF byte vector using headless Chromium.
///
/// The caller is responsible for ensuring Chromium is installed.
/// Returns an error if no Chromium binary is found or if Chromium exits non-zero.
pub async fn html_to_pdf(html: &str) -> Result<Vec<u8>> {
    let chrome = find_chrome().context(
        "No Chromium binary found. Install chromium, chromium-browser, or google-chrome."
    )?;

    let tmp = tempfile::tempdir().context("Failed to create temp dir")?;
    let html_path = tmp.path().join("schedule.html");
    let pdf_path  = tmp.path().join("schedule.pdf");

    tokio::fs::write(&html_path, html.as_bytes()).await
        .context("Failed to write temp HTML")?;

    let out = tokio::process::Command::new(&chrome)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--disable-gpu",
            "--run-all-compositor-stages-before-draw",
            "--print-to-pdf-no-header",
            &format!("--print-to-pdf={}", pdf_path.display()),
            &format!("file://{}", html_path.display()),
        ])
        .output()
        .await
        .with_context(|| format!("Failed to spawn {chrome}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("Chromium exited {}: {}", out.status, stderr.trim());
    }

    tokio::fs::read(&pdf_path).await
        .context("PDF output file not found after Chromium run")
}

fn find_chrome() -> Option<String> {
    CHROME_CANDIDATES.iter().find_map(|&bin| {
        std::process::Command::new("which")
            .arg(bin)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| bin.to_owned())
    })
}
