//! Maps a working directory to a project id: the normalized `origin` remote
//! (`github.com/owner/repo`) when there is one, otherwise `local/<folder>`.

use std::fs;
use std::path::{Path, PathBuf};

pub fn resolve(cwd: &Path) -> String {
    let Some(root) = git_root(cwd) else {
        return local_id(cwd);
    };
    git_config_path(&root)
        .and_then(|config| fs::read_to_string(config).ok())
        .and_then(|config| origin_url(&config))
        .and_then(|url| normalize_remote(&url))
        .unwrap_or_else(|| local_id(&root))
}

fn local_id(dir: &Path) -> String {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into());
    format!("local/{name}")
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn git_config_path(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("config"));
    }
    // worktrees and submodules: `.git` is a file pointing at the real git dir
    let pointer = fs::read_to_string(&dot_git).ok()?;
    let git_dir = root.join(pointer.trim().strip_prefix("gitdir:")?.trim());
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(relative) => git_dir.join(relative.trim()),
        Err(_) => git_dir,
    };
    Some(common.join("config"))
}

fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
        } else if in_origin
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "url"
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// `git@github.com:Owner/Repo.git`, `https://user:token@github.com/Owner/Repo`
/// and `ssh://git@github.com/Owner/Repo` all become `github.com/Owner/Repo`.
/// Credentials in the URL are dropped, never stored.
fn normalize_remote(url: &str) -> Option<String> {
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, after)| after);
        rest.split_once('/')?
    } else {
        let rest = url.rsplit_once('@').map_or(url, |(_, after)| after);
        rest.split_once(':')?
    };
    let host = host.split(':').next()?.to_ascii_lowercase();
    let path = path.trim_matches('/').trim_end_matches(".git");
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_remote_forms() {
        for url in [
            "git@github.com:hackclub/pulse.git",
            "https://github.com/hackclub/pulse",
            "https://someone:ghp_secret@GitHub.com/hackclub/pulse.git",
            "ssh://git@github.com:22/hackclub/pulse.git",
        ] {
            assert_eq!(
                normalize_remote(url).as_deref(),
                Some("github.com/hackclub/pulse"),
                "{url}"
            );
        }
    }

    #[test]
    fn resolves_subdirectories_worktrees_and_plain_folders() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/feature")).unwrap();
        fs::create_dir_all(repo.join("src/deep")).unwrap();
        fs::write(
            repo.join(".git/config"),
            "[core]\n\turl = nope\n[remote \"upstream\"]\n\turl = git@github.com:other/x.git\n[remote \"origin\"]\n\turl = git@github.com:rowan/cs2-web.git\n",
        )
        .unwrap();
        fs::write(repo.join(".git/worktrees/feature/commondir"), "../..\n").unwrap();

        let worktree = dir.path().join("feature");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                repo.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();

        let plain = dir.path().join("scratch");
        fs::create_dir_all(&plain).unwrap();

        assert_eq!(resolve(&repo.join("src/deep")), "github.com/rowan/cs2-web");
        assert_eq!(resolve(&worktree), "github.com/rowan/cs2-web");
        assert_eq!(resolve(&plain), "local/scratch");
    }
}
