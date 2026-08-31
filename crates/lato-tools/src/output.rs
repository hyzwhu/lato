use std::path::{Path, PathBuf};

pub const TOOL_OUTPUT_LIMIT_BYTES: usize = 20_000;

pub async fn bound_tool_output(
    output: String,
    cwd: &Path,
    call_id: &str,
) -> Result<String, String> {
    if output.len() <= TOOL_OUTPUT_LIMIT_BYTES {
        return Ok(output);
    }
    let safe_id: String = call_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let dir = cwd.join(".lato").join("tool-output");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let path: PathBuf = dir.join(format!(
        "{}.txt",
        if safe_id.is_empty() {
            "output"
        } else {
            &safe_id
        }
    ));
    tokio::fs::write(&path, output.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut boundary = TOOL_OUTPUT_LIMIT_BYTES;
    while !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    Ok(format!(
        "{}\n\n[tool output truncated; full output: {}]",
        &output[..boundary],
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn long_tool_output_is_bounded_and_spilled_to_file() {
        let d = tempfile::tempdir().unwrap();
        let output = "x".repeat(TOOL_OUTPUT_LIMIT_BYTES + 1_000);
        let bounded = bound_tool_output(output.clone(), d.path(), "call-1")
            .await
            .unwrap();
        assert!(bounded.len() < output.len());
        assert_eq!(
            std::fs::read_to_string(d.path().join(".lato/tool-output/call-1.txt")).unwrap(),
            output
        );
    }
}
