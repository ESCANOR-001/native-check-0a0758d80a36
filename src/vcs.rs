//! Bounded Git helpers. Inspection is local; an explicit worktree request may fetch origin.
use crate::chat_projection::{now, s};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
const LIMIT: usize = 1024 * 1024;

/// Git for Windows rejects `//?/C:/...` when creating a worktree's .git file.
/// Keep canonical paths for authorization/storage, but pass Git a normal path.
pub(crate) fn path_argument(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or("Git path is not UTF-8")?;
    #[cfg(windows)]
    {
        windows_path_argument(text)
    }
    #[cfg(not(windows))]
    {
        Ok(text.to_owned())
    }
}

#[cfg(any(windows, test))]
fn windows_path_argument(text: &str) -> Result<String, String> {
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return Ok(format!("//{}", rest.replace('\\', "/")));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let drive = rest.as_bytes();
        if drive.len() < 3 || !drive[0].is_ascii_alphabetic() || &drive[1..3] != b":\\" {
            return Err("Git does not support this Windows device path".into());
        }
        return Ok(rest.replace('\\', "/"));
    }
    Ok(text.replace('\\', "/"))
}

pub(crate) async fn git(root: &Path, args: &[&str]) -> Result<(i32, Vec<u8>), String> {
    git_with_timeout(root, args, Duration::from_secs(8), false).await
}
async fn git_with_timeout(
    root: &Path,
    args: &[&str],
    timeout: Duration,
    network: bool,
) -> Result<(i32, Vec<u8>), String> {
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    cmd.args(["-c", "core.longpaths=true"]);
    cmd.args([
        "--no-pager",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=",
        "-c",
        "diff.external=",
    ])
    .args(args)
    .current_dir(root)
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_OPTIONAL_LOCKS", "0")
    .env("LC_ALL", "C");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        cmd.env_remove(name);
    }
    if network {
        // Never open a login/browser/SSH password prompt from a phone request.
        // Existing noninteractive credentials and SSH agents can still be used.
        cmd.env("GCM_INTERACTIVE", "never")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
            .env("GIT_SSH_VARIANT", "ssh");
    }
    let result = crate::subprocess::run(cmd, timeout, LIMIT, 8192).await?;
    Ok((result.code, result.stdout))
}

pub(crate) async fn fetch_origin(root: &Path, reference: &str) -> Result<String, String> {
    let branch = reference
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| reference.strip_prefix("origin/"))
        .or_else(|| reference.strip_prefix("refs/heads/"))
        .unwrap_or(reference);
    if branch.is_empty()
        || branch.len() > 256
        || branch.starts_with('-')
        || branch.starts_with("refs/")
        || git(root, &["check-ref-format", &format!("refs/heads/{branch}")])
            .await?
            .0
            != 0
    {
        return Err("Select a branch on origin to fetch".into());
    }
    let remote = format!("refs/remotes/origin/{branch}");
    let spec = format!("+refs/heads/{branch}:{remote}");
    // Restrict the update to the selected remote-tracking ref: never the user's
    // checked-out branch/index, tags, submodules or configured fetch mappings.
    let result = git_with_timeout(
        root,
        &[
            "-c",
            "credential.interactive=false",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--no-write-fetch-head",
            "--refmap=",
            "origin",
            &spec,
        ],
        Duration::from_secs(60),
        true,
    )
    .await
    .map_err(|_| "Fetching origin timed out or failed; check the PC's Git connection")?;
    if result.0 != 0 {
        return Err(
            "Could not fetch this branch from origin. Check the branch and Git sign-in on the PC"
                .into(),
        );
    }
    Ok(remote)
}
async fn output(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let (code, bytes) = git(root, args).await?;
    if code != 0 {
        return Err("Git could not inspect this repository".into());
    }
    Ok(bytes)
}
pub async fn inspect(base: &Path, method: &str, p: &Value) -> Result<Value, String> {
    let root = crate::workspace::resolve(base, s(&p["cwd"]))?;
    let (repo, top) = git(&root, &["rev-parse", "--show-toplevel"]).await?;
    if repo != 0 {
        return if method == "vcs.listRefs" {
            Ok(
                json!({"refs":[],"isRepo":false,"hasPrimaryRemote":false,"nextCursor":null,"totalCount":0}),
            )
        } else if method == "vcs.refreshStatus" {
            Ok(empty_status())
        } else {
            Err("This folder is not a Git repository".into())
        };
    }
    crate::workspace::resolve(base, String::from_utf8_lossy(&top).trim())?;
    match method {
        "vcs.refreshStatus" => status(&root).await,
        "vcs.listRefs" => refs(&root, p).await,
        "review.getDiffPreview" => diff(&root, p["ignoreWhitespace"] == true).await,
        _ => Err("Unsupported Git inspection".into()),
    }
}
fn empty_status() -> Value {
    json!({"isRepo":false,"hasPrimaryRemote":false,"isDefaultRef":false,"refName":null,"hasWorkingTreeChanges":false,"workingTree":{"files":[],"insertions":0,"deletions":0},"hasUpstream":false,"aheadCount":0,"behindCount":0,"pr":null})
}
async fn refs(root: &Path, p: &Value) -> Result<Value, String> {
    let bytes = output(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(HEAD)%00%(symref)",
            "refs/heads/",
            "refs/remotes/",
        ],
    )
    .await?;
    let text = String::from_utf8(bytes).map_err(|_| "Branch names are not UTF-8")?;
    let query = s(&p["query"]).to_lowercase();
    if query.len() > 256 {
        return Err("Search is too long".into());
    }
    let mut rows = vec![];
    let mut default = None;
    for line in text.lines() {
        let parts: Vec<_> = line.split('\0').collect();
        if parts.len() != 3 {
            continue;
        }
        if parts[0] == "refs/remotes/origin/HEAD" {
            default = parts[2]
                .strip_prefix("refs/remotes/origin/")
                .map(str::to_owned);
        }
    }
    for line in text.lines() {
        let parts: Vec<_> = line.split('\0').collect();
        if parts.len() != 3 || !parts[2].is_empty() {
            continue;
        }
        let remote = parts[0].starts_with("refs/remotes/");
        let name = parts[0]
            .strip_prefix(if remote {
                "refs/remotes/"
            } else {
                "refs/heads/"
            })
            .unwrap_or(parts[0]);
        if (p["refKind"] == "local" && remote)
            || (p["refKind"] == "remote" && !remote)
            || !name.to_lowercase().contains(&query)
        {
            continue;
        }
        rows.push(json!({"name":name,"isRemote":remote,"current":parts[1]=="*","isDefault":default.as_deref()==Some(name),"worktreePath":if parts[1]=="*" {json!(root)} else {Value::Null}}));
    }
    let total = rows.len();
    let cursor = p["cursor"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
    let limit = p["limit"].as_u64().unwrap_or(100).clamp(1, 200) as usize;
    let next = cursor.saturating_add(limit);
    let page: Vec<_> = rows.into_iter().skip(cursor).take(limit).collect();
    Ok(
        json!({"refs":page,"isRepo":true,"hasPrimaryRemote":!output(root,&["remote"]).await?.is_empty(),"totalCount":total,"nextCursor":if next<total {json!(next)} else {Value::Null}}),
    )
}
async fn status(root: &Path) -> Result<Value, String> {
    let mut result = empty_status();
    result["isRepo"] = json!(true);
    let (code, name) = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    if code == 0 {
        result["refName"] = json!(String::from_utf8_lossy(&name).trim());
    }
    result["hasPrimaryRemote"] = json!(!output(root, &["remote"]).await?.is_empty());
    let (code, default) = git(
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await?;
    if code == 0 {
        result["isDefaultRef"] = json!(
            String::from_utf8_lossy(&default)
                .trim()
                .strip_prefix("refs/remotes/origin/")
                == result["refName"].as_str()
        );
    }
    let raw = output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    result["hasWorkingTreeChanges"] = json!(!raw.is_empty());
    let mut files = BTreeMap::<String, (u64, u64)>::new();
    let mut untracked = Vec::new();
    let mut records = raw.split(|b| *b == 0).filter(|b| !b.is_empty());
    while let Some(row) = records.next() {
        if row.len() < 4 {
            continue;
        }
        let path = String::from_utf8(row[3..].to_vec()).map_err(|_| "File paths are not UTF-8")?;
        if &row[..2] == b"??" {
            untracked.push(path.clone());
        }
        if row[..2].iter().any(|c| *c == b'R' || *c == b'C') {
            records.next();
        }
        files.insert(path, (0, 0));
    }
    let head = git(root, &["rev-parse", "--verify", "HEAD"]).await?.0 == 0;
    let args = if head {
        vec![
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
            ".",
        ]
    } else {
        vec![
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--",
            ".",
        ]
    };
    let stats = output(root, &args).await?;
    for row in stats.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let mut parts = row.splitn(3, |b| *b == b'\t');
        let adds = parts.next().unwrap_or_default();
        let dels = parts.next().unwrap_or_default();
        if let Some(path) = parts.next() {
            files.insert(
                String::from_utf8(path.to_vec()).map_err(|_| "File paths are not UTF-8")?,
                (
                    String::from_utf8_lossy(adds).parse().unwrap_or(0),
                    String::from_utf8_lossy(dels).parse().unwrap_or(0),
                ),
            );
        }
    }
    // With no HEAD, all existing files are additions against an empty tree.
    // Count the working content, including edits after staging. Never follow
    // symlinks or read an unbounded new file just to populate a status badge.
    let count_paths = if head {
        untracked
    } else {
        files.keys().cloned().collect()
    };
    let mut budget = 4 * LIMIT;
    let mut counts_truncated = false;
    for path in count_paths {
        match new_file_lines(root, &path, &mut budget).await? {
            Some(lines) => {
                files.insert(path, (lines, 0));
            }
            None => {
                counts_truncated = true;
            }
        }
    }
    let additions: u64 = files.values().map(|v| v.0).sum();
    let deletions: u64 = files.values().map(|v| v.1).sum();
    let rows: Vec<_> = files
        .into_iter()
        .map(|(path, (a, d))| json!({"path":path,"insertions":a,"deletions":d}))
        .collect();
    result["workingTree"] = json!({"files":rows,"insertions":additions,"deletions":deletions,"countsTruncated":counts_truncated});
    let (code, counts) = git(
        root,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )
    .await?;
    if code == 0 {
        let counts = String::from_utf8_lossy(&counts);
        let n: Vec<_> = counts.split_whitespace().collect();
        if n.len() == 2 {
            result["hasUpstream"] = json!(true);
            result["aheadCount"] = json!(n[0].parse::<u64>().unwrap_or(0));
            result["behindCount"] = json!(n[1].parse::<u64>().unwrap_or(0));
        }
    }
    Ok(result)
}
async fn new_file_lines(
    root: &Path,
    path: &str,
    budget: &mut usize,
) -> Result<Option<u64>, String> {
    let file = root.join(path);
    let meta = match tokio::fs::symlink_metadata(&file).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(0)),
        Err(_) => return Err("Cannot inspect new file".into()),
    };
    if meta.file_type().is_symlink() {
        return Ok(Some(1));
    }
    if !meta.is_file() || meta.len() > LIMIT as u64 || meta.len() > *budget as u64 {
        return Ok(None);
    }
    crate::workspace::resolve(root, path)?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(file)
        .await
        .map_err(|_| "Cannot read new file")?
        .take((*budget).min(LIMIT) as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "Cannot read new file")?;
    if bytes.len() > LIMIT || bytes.len() > *budget {
        return Ok(None);
    }
    *budget -= bytes.len();
    if bytes.contains(&0) {
        return Ok(Some(0));
    }
    Ok(Some(
        bytes.iter().filter(|b| **b == b'\n').count() as u64
            + u64::from(!bytes.is_empty() && bytes.last() != Some(&b'\n')),
    ))
}
async fn diff(root: &Path, ignore: bool) -> Result<Value, String> {
    let head = git(root, &["rev-parse", "--verify", "HEAD"]).await?.0 == 0;
    let mut args = vec![
        "diff",
        "--relative",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ];
    if ignore {
        args.push("--ignore-all-space");
    }
    if head {
        args.push("HEAD");
    } else {
        args.push("--cached");
    }
    args.extend(["--", "."]);
    let mut bytes = output(root, &args).await?;
    if !head {
        args.retain(|v| *v != "--cached");
        bytes.extend(output(root, &args).await?);
    }
    let untracked = output(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )
    .await?;
    let names: Vec<_> = untracked
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .collect();
    let mut truncated = names.len() > 100;
    for name in names.into_iter().take(100) {
        let name = std::str::from_utf8(name).map_err(|_| "File paths are not UTF-8")?;
        if crate::workspace::resolve(root, name).is_err() {
            continue;
        }
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let (code, extra) = git(
            root,
            &[
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--",
                null,
                name,
            ],
        )
        .await?;
        if code != 0 && code != 1 {
            return Err("Cannot preview new file".into());
        }
        if bytes.len() + extra.len() > LIMIT {
            truncated = true;
            break;
        }
        bytes.extend(extra);
    }
    if bytes.len() > LIMIT {
        bytes.truncate(LIMIT);
        truncated = true;
    }
    let diff = String::from_utf8_lossy(&bytes);
    Ok(
        json!({"cwd":root,"generatedAt":now(),"sources":[{"id":"working-tree","kind":"working-tree","title":"Uncommitted changes","baseRef":if head {Some("HEAD")} else {None},"headRef":null,"diff":diff,"diffHash":format!("{:x}",Sha256::digest(&bytes)),"truncated":truncated}]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_git_arguments_preserve_drive_unc_spaces_and_unicode() {
        for (input, expected) in [
            (
                r"\\?\C:\Users\Example\project with spaces",
                "C:/Users/Example/project with spaces",
            ),
            (r"\\?\UNC\server\share\日本語", "//server/share/日本語"),
            (r"C:\normal\path", "C:/normal/path"),
            (r"\\server\share\path", "//server/share/path"),
        ] {
            assert_eq!(windows_path_argument(input).unwrap(), expected);
        }
        assert!(windows_path_argument(r"\\?\GLOBALROOT\Device\HarddiskVolume1").is_err());
        #[cfg(not(windows))]
        assert_eq!(
            path_argument(Path::new(r"/a/back\slash")).unwrap(),
            r"/a/back\slash"
        );
    }

    #[tokio::test]
    async fn git_creates_worktree_from_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project with spaces");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        output(&root, &["init", "-b", "main"]).await.unwrap();
        output(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .await
        .unwrap();
        let target = dir.path().canonicalize().unwrap().join("isolated checkout");
        output(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "fixture-checkout",
                "--",
                &path_argument(&target).unwrap(),
                "HEAD",
            ],
        )
        .await
        .unwrap();
        let actual = output(&target, &["rev-parse", "--show-toplevel"])
            .await
            .unwrap();
        assert_eq!(
            Path::new(std::str::from_utf8(&actual).unwrap().trim())
                .canonicalize()
                .unwrap(),
            target.canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn status_diff_and_refs_use_only_the_disposable_repository() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        assert_eq!(
            inspect(root, "vcs.refreshStatus", &json!({"cwd":root}))
                .await
                .unwrap()["isRepo"],
            false
        );
        output(root, &["init", "-b", "main"]).await.unwrap();
        std::fs::write(root.join("file name.txt"), "one\n").unwrap();
        output(root, &["add", "--", "file name.txt"]).await.unwrap();
        output(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "Initial",
            ],
        )
        .await
        .unwrap();
        std::fs::write(root.join("file name.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let state = inspect(root, "vcs.refreshStatus", &json!({"cwd":root}))
            .await
            .unwrap();
        assert_eq!(state["refName"], "main");
        assert_eq!(state["workingTree"]["insertions"], 2);
        assert_eq!(state["workingTree"]["files"].as_array().unwrap().len(), 2);
        let view = inspect(root, "review.getDiffPreview", &json!({"cwd":root}))
            .await
            .unwrap();
        let text = s(&view["sources"][0]["diff"]);
        assert!(text.contains("+two"));
        assert!(text.contains("+new"));
        let branches = inspect(root, "vcs.listRefs", &json!({"cwd":root}))
            .await
            .unwrap();
        assert_eq!(branches["refs"][0]["name"], "main");
        assert_eq!(branches["refs"][0]["current"], true);
        assert!(
            inspect(root, "vcs.refreshStatus", &json!({"cwd":"../outside"}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn unborn_status_counts_working_content_and_bounds_large_files() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        output(root, &["init", "-b", "main"]).await.unwrap();
        std::fs::write(root.join("staged.txt"), "one\n").unwrap();
        output(root, &["add", "--", "staged.txt"]).await.unwrap();
        std::fs::write(root.join("staged.txt"), "one\ntwo").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        std::fs::write(root.join("big.txt"), vec![b'x'; LIMIT + 1]).unwrap();
        let state = status(root).await.unwrap();
        assert_eq!(state["workingTree"]["insertions"], 3);
        assert_eq!(state["workingTree"]["countsTruncated"], true);
    }
}
