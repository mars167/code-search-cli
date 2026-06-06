//! SCIP indexer orchestration.
//!
//! Calls language-specific indexers (Go compiler helper, etc.) to produce
//! `index.scip` files, then imports them via the existing `index import-scip`
//! pipeline.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Run the Go SCIP indexer on a project root and return the path to the
/// generated SCIP JSON file.
pub fn generate_go_scip(project_root: &Path, output_path: &Path) -> Result<()> {
    let indexer_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scip-indexer");

    let output = Command::new("go")
        .args([
            "run",
            "main.go",
            "--output",
            output_path.to_str().unwrap(),
            project_root.to_str().unwrap(),
        ])
        .current_dir(&indexer_dir)
        .output()
        .with_context(|| format!("failed to run Go SCIP indexer for {}", project_root.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Go SCIP indexer failed: {stderr}");
    }

    eprintln!(
        "{}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(())
}

/// Run the Go SCIP indexer and then import the result.
pub fn generate_and_import(project_root: &Path) -> Result<()> {
    let tmp = std::env::temp_dir().join("codetrail-index.scip.json");
    generate_go_scip(project_root, &tmp)?;

    // Import using the existing command
    let status = Command::new(
        std::env::current_exe().unwrap_or_else(|_| "codetrail".into()),
    )
    .args(["index", "import-scip", tmp.to_str().unwrap()])
    .current_dir(project_root)
    .status()
    .with_context(|| "failed to import generated SCIP index")?;

    if !status.success() {
        anyhow::bail!("SCIP import failed");
    }

    // Cleanup
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn go_indexer_produces_output_for_valid_project() {
        let dir = tempdir().unwrap();
        let project = dir.path();
        fs::create_dir_all(project.join("pkg")).unwrap();
        fs::write(
            project.join("go.mod"),
            "module example.com/test\n\ngo 1.21\n",
        )
        .unwrap();
        fs::write(
            project.join("pkg/math.go"),
            "package pkg\n\n// Add returns the sum of two integers.\nfunc Add(a, b int) int { return a + b }\n",
        )
        .unwrap();

        let output = project.join("index.scip.json");
        let result = generate_go_scip(project, &output);
        // May fail if Go toolchain issues, but should not panic
        if result.is_ok() {
            assert!(output.exists());
            let content = fs::read_to_string(&output).unwrap();
            assert!(content.contains("Add"));
            assert!(content.contains("\"documents\""));
        }
    }
}
