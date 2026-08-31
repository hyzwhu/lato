use regex::Regex;
use std::{fs, path::Path};

pub async fn read_file(
    path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| e.to_string())?;
    let lines: Vec<&str> = text.lines().collect();
    if offset.is_none() && limit.is_none() {
        return Ok(text);
    }
    let start = offset.unwrap_or(1).saturating_sub(1);
    let end = limit
        .map(|l| start + l)
        .unwrap_or(lines.len())
        .min(lines.len());
    Ok(lines[start.min(lines.len())..end].join("\n"))
}

pub async fn list_dir(path: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(path).await.map_err(|e| e.to_string())?;
    while let Some(e) = rd.next_entry().await.map_err(|e| e.to_string())? {
        out.push(e.file_name().to_string_lossy().into_owned());
    }
    out.sort();
    Ok(out)
}

pub fn grep(root: &Path, pattern: &str) -> Result<Vec<String>, String> {
    let re = Regex::new(pattern).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    walk(root, &re, &mut out)?;
    Ok(out)
}

fn walk(path: &Path, re: &Regex, out: &mut Vec<String>) -> Result<(), String> {
    if path.file_name().and_then(|s| s.to_str()) == Some(".git") {
        return Ok(());
    }
    if path.is_dir() {
        for e in fs::read_dir(path).map_err(|e| e.to_string())? {
            walk(&e.map_err(|e| e.to_string())?.path(), re, out)?;
        }
    } else if path.is_file()
        && let Ok(text) = fs::read_to_string(path)
    {
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                out.push(format!("{}:{}:{}", path.display(), i + 1, line));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn read_file_offset() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.txt");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        let s = read_file(&p, Some(2), Some(1)).await.unwrap();
        assert!(s.contains('b'));
        assert!(!s.contains('c'));
    }
}
