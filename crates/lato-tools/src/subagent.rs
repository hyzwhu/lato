use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct SubagentWorktree {
    pub path: PathBuf,
    pub branch: String,
}

pub async fn create_subagent_worktree(
    repo: &Path,
    worktrees_root: &Path,
    session_id: &str,
) -> Result<SubagentWorktree, String> {
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("invalid subagent session id".into());
    }
    tokio::fs::create_dir_all(worktrees_root)
        .await
        .map_err(|e| e.to_string())?;
    let branch = format!("lato/subagent-{session_id}");
    let path = worktrees_root.join(session_id);
    let output = tokio::process::Command::new("git")
        .current_dir(repo)
        .args(["worktree", "add", "-b", &branch])
        .arg(&path)
        .arg("HEAD")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(SubagentWorktree { path, branch })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn e5_1_parallel_subagent_gets_git_worktree_isolation() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        let roots = d.path().join("worktrees");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "lato@example.invalid"]);
        run(&["config", "user.name", "Lato Test"]);
        std::fs::write(repo.join("README.md"), "root").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "initial"]);
        let worktree = create_subagent_worktree(&repo, &roots, "one")
            .await
            .unwrap();
        std::fs::write(worktree.path.join("README.md"), "subagent").unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "root"
        );
        assert_ne!(worktree.path, repo);
    }
}
