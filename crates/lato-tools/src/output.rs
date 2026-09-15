use std::path::{Path, PathBuf};

pub const TOOL_OUTPUT_LIMIT_BYTES: usize = 20_000;

/// Result of bounding oversized tool output (M-15).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedToolOutput {
    pub content: String,
    pub truncated: bool,
    pub artifact_path: Option<String>,
}

pub async fn bound_tool_output(
    output: String,
    cwd: &Path,
    call_id: &str,
) -> Result<String, String> {
    Ok(bound_tool_output_detailed(output, cwd, call_id)
        .await?
        .content)
}

/// Bound oversized tool output, spilling the full body under `.lato/tool-output/`.
///
/// Returns truncated inline content plus the spill path when the limit is exceeded.
pub async fn bound_tool_output_detailed(
    output: String,
    cwd: &Path,
    call_id: &str,
) -> Result<BoundedToolOutput, String> {
    if output.len() <= TOOL_OUTPUT_LIMIT_BYTES {
        return Ok(BoundedToolOutput {
            content: output,
            truncated: false,
            artifact_path: None,
        });
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
    Ok(BoundedToolOutput {
        content: format!(
            "{}\n\n[tool output truncated; full output: {}]",
            &output[..boundary],
            path.display()
        ),
        truncated: true,
        artifact_path: Some(path.display().to_string()),
    })
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

    #[tokio::test]
    async fn detailed_bound_marks_truncated_and_artifact_path() {
        let d = tempfile::tempdir().unwrap();
        let output = "y".repeat(TOOL_OUTPUT_LIMIT_BYTES + 50);
        let bounded = bound_tool_output_detailed(output.clone(), d.path(), "mcp-call")
            .await
            .unwrap();
        assert!(bounded.truncated);
        let path = bounded.artifact_path.expect("spill path");
        assert!(
            path.replace(std::path::MAIN_SEPARATOR, "/")
                .contains(".lato/tool-output/mcp-call.txt")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), output);
        assert!(bounded.content.contains("[tool output truncated"));
        assert!(
            !bounded
                .content
                .contains(&"y".repeat(TOOL_OUTPUT_LIMIT_BYTES + 50))
        );
    }
}
