use super::*;

pub(crate) fn git_paths(root: &Path) -> anyhow::Result<GitPaths> {
    let worktree_root = git_rev_parse(root, "--show-toplevel")?;
    let git_dir = git_rev_parse(root, "--git-dir")?;
    let git_common_dir = git_rev_parse(root, "--git-common-dir")?;
    let hooks_dir = git_rev_parse(root, "--git-path hooks")?;
    Ok(GitPaths { worktree_root, git_dir, git_common_dir, hooks_dir })
}
pub(crate) fn git_rev_parse(root: &Path, arg: &str) -> anyhow::Result<PathBuf> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).arg("rev-parse");
    for part in arg.split_whitespace() {
        command.arg(part);
    }
    let output = command.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse {arg} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value = String::from_utf8(output.stdout)?.trim().to_string();
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() { path } else { root.join(path) })
}
pub(crate) fn install_hook(hooks_dir: &Path, hook: &str) -> anyhow::Result<()> {
    let path = hooks_dir.join(hook);
    if path.exists() && !is_rag_rat_hook(&path)? {
        anyhow::bail!(
            "{} already exists and is not managed by rag-rat; move it aside or merge manually",
            path.display()
        );
    }
    fs::write(&path, hook_script(hook))?;
    make_executable(&path)?;
    Ok(())
}
pub(crate) fn is_rag_rat_hook(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)?.contains(HOOK_MARKER))
}
pub(crate) fn hook_script(hook: &str) -> String {
    let command = match hook {
        "post-checkout" =>
            r#"rag-rat maintenance \
    --trigger post-checkout \
    --old-head "$1" \
    --new-head "$2" \
    --branch-checkout "$3" \
    --max-seconds 30"#,
        "post-merge" =>
            r#"rag-rat maintenance \
    --trigger post-merge \
    --max-seconds 30 \
    "$@""#,
        "post-rewrite" =>
            r#"rag-rat maintenance \
    --trigger post-rewrite \
    --max-seconds 30 \
    "$@""#,
        // git passes no positional args to post-commit; HEAD has already advanced, so the
        // maintenance discover-index re-keys the just-committed files under the new commit.
        "post-commit" =>
            r#"rag-rat maintenance \
    --trigger post-commit \
    --max-seconds 30"#,
        _ => unreachable!("unknown managed hook"),
    };
    format!(
        r#"#!/bin/sh
{HOOK_MARKER} Edit rag-rat config, not this hook.

if [ "${{RAG_RAT_HOOK_DISABLE:-}}" = "1" ]; then
  exit 0
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
cd "$repo_root" || exit 0

RAG_RAT_HOOK_DISABLE=1 \
  {command} >"${{TMPDIR:-/tmp}}/rag-rat-{hook}.log" 2>&1 &

exit 0
"#
    )
}
#[cfg(unix)]
pub(crate) fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn make_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}
