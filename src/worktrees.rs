//! Durable intent before Git mutation; retries recover the same checkout.
//! The original checkout, index and uncommitted files are never changed.
use crate::{
    auth_service::AuthService,
    chat::Chat,
    chat_projection::{now, s},
    store::Store,
    vcs::git,
    workspace,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SetupStage {
    #[default]
    Planned,
    Running,
    Failed,
    Done,
}

pub(crate) struct Setup {
    pub project_root: PathBuf,
    pub scripts: Vec<Value>,
}
pub(crate) enum Target {
    Create(Option<Value>),
    Existing {
        path: String,
        branch: Option<String>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    repo: PathBuf,
    common: PathBuf,
    head: String,
    branch: String,
    path: PathBuf,
    chat: Chat,
    #[serde(default)]
    scripts: Vec<Value>,
    #[serde(default)]
    project_root: Option<PathBuf>,
    #[serde(default)]
    setup: SetupStage,
    #[serde(default)]
    existing: bool,
}
impl Store {
    pub(crate) fn worktree_intent(&self, key: &str) -> rusqlite::Result<Option<(String, String)>> {
        self.db
            .query_row(
                "SELECT digest,data FROM native_worktrees WHERE id=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }
    fn reserve_worktree(&mut self, key: &str, digest: &str, data: &str) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        let count: i64 = tx.query_row("SELECT count(*) FROM native_worktrees", [], |r| r.get(0))?;
        if count >= 128 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        tx.execute(
            "INSERT INTO native_worktrees(id,digest,data) VALUES(?1,?2,?3)",
            params![key, digest, data],
        )?;
        tx.commit()
    }
    fn update_worktree(&self, key: &str, digest: &str, data: &str) -> rusqlite::Result<()> {
        if self.db.execute(
            "UPDATE native_worktrees SET data=?3 WHERE id=?1 AND digest=?2",
            params![key, digest, data],
        )? != 1
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(())
    }
}
async fn text(root: &Path, args: &[&str]) -> Result<String, String> {
    let (code, bytes) = git(root, args).await?;
    if code != 0 {
        return Err(format!(
            "Git could not prepare the isolated checkout ({} exited {code})",
            args.first().copied().unwrap_or("inspection")
        ));
    }
    String::from_utf8(bytes)
        .map(|s| s.trim().to_owned())
        .map_err(|_| "Git path is not UTF-8".into())
}
async fn current_branch(root: &Path) -> Result<String, String> {
    let (code, bytes) = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    match code {
        0 => String::from_utf8(bytes)
            .map(|s| s.trim().to_owned())
            .map_err(|_| "Git branch is not UTF-8".into()),
        1 => Ok(String::new()), // Detached worktrees are valid explicit destinations.
        _ => Err("Cannot inspect the checkout branch".into()),
    }
}
async fn existing(
    base: &Path,
    mut chat: Chat,
    path: &str,
    branch: Option<&str>,
) -> Result<Intent, String> {
    let root = workspace::resolve(base, s(&chat.thread["worktreePath"]))?;
    let repo = workspace::resolve(base, &text(&root, &["rev-parse", "--show-toplevel"]).await?)?;
    let common = workspace::resolve(
        base,
        &text(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )?;
    let path = workspace::resolve(base, path)?;
    if path != workspace::resolve(base, &text(&path, &["rev-parse", "--show-toplevel"]).await?)?
        || common
            != workspace::resolve(
                base,
                &text(
                    &path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .await?,
            )?
    {
        return Err("Select a registered worktree belonging to this project".into());
    }
    let actual = current_branch(&path).await?;
    if branch.is_some_and(|name| name != actual) {
        return Err("The selected worktree branch changed; refresh the project picker".into());
    }
    let head = text(&path, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    chat.thread["worktreePath"] = json!(path);
    chat.thread["branch"] = if actual.is_empty() {
        Value::Null
    } else {
        json!(actual)
    };
    Ok(Intent {
        repo,
        common,
        head,
        branch: actual,
        path,
        chat,
        scripts: vec![],
        project_root: Some(root),
        setup: SetupStage::Planned,
        existing: true,
    })
}
async fn plan(base: &Path, source: Chat, spec: Option<&Value>) -> Result<Intent, String> {
    let cwd = workspace::resolve(base, s(&source.thread["worktreePath"]))?;
    let repo = workspace::resolve(base, &text(&cwd, &["rev-parse", "--show-toplevel"]).await?)?;
    let common = workspace::resolve(
        base,
        &text(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let id = if spec.is_some() {
        s(&source.thread["id"]).to_owned()
    } else {
        nonce.clone()
    };
    let mut reference = "HEAD".to_owned();
    let mut branch = format!("remodex/{}-{}", source.agent.driver(), &nonce[..12]);
    if let Some(spec) = spec {
        if !spec.is_object()
            || spec.as_object().unwrap().keys().any(|k| {
                !["projectCwd", "baseBranch", "branch", "startFromOrigin"].contains(&k.as_str())
            })
        {
            return Err("Invalid worktree preparation".into());
        }
        if workspace::resolve(base, s(&spec["projectCwd"]))? != cwd {
            return Err("The worktree project folder does not match the selected project".into());
        }
        if spec.get("startFromOrigin").is_some_and(|v| !v.is_boolean()) {
            return Err("Invalid fetch preference".into());
        }
        reference = s(&spec["baseBranch"]).to_owned();
        if reference.is_empty() || reference.len() > 256 || reference.chars().any(char::is_control)
        {
            return Err("Select a bounded base reference".into());
        }
        if let Some(requested) = spec.get("branch") {
            let name = requested.as_str().ok_or("Invalid branch name")?;
            if name.is_empty()
                || name.len() > 256
                || name.starts_with('-')
                || name.chars().any(char::is_control)
                || git(&repo, &["check-ref-format", "--branch", name]).await?.0 != 0
            {
                return Err("Invalid new branch name".into());
            }
            branch = name.to_owned();
        }
    }
    if git(
        &repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await?
    .0 == 0
    {
        return Err("The requested branch already exists; choose another name".into());
    }
    if spec.is_some_and(|spec| spec["startFromOrigin"] == true) {
        reference = crate::vcs::fetch_origin(&repo, &reference).await?;
    }
    let head = text(
        &repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await?;
    let candidate = base
        .canonicalize()
        .map_err(|_| "Workspace unavailable")?
        .join(".remodex-worktrees");
    if candidate.starts_with(&repo) {
        return Err("The isolated workspace must contain space outside this repository for a separate checkout".into());
    }
    let create_parent = !candidate.exists();
    let parent = workspace::directory(base, ".remodex-worktrees", true)?;
    if parent.starts_with(&repo) {
        return Err("The worktree directory must be outside the source repository".into());
    }
    #[cfg(unix)]
    if create_parent {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect the worktree directory")?;
    }
    #[cfg(not(unix))]
    let _ = create_parent;
    let path = parent.join(&nonce);
    let time = now();
    let mut chat = source;
    chat.detach_native_session();
    chat.native_id = if chat.agent == crate::chat::AgentKind::Claude {
        uuid::Uuid::new_v4().to_string()
    } else {
        String::new()
    };
    let t = &mut chat.thread;
    for field in ["messages", "activities", "proposedPlans", "checkpoints"] {
        t[field] = json!([]);
    }
    for field in [
        "latestTurn",
        "latestUserMessageAt",
        "archivedAt",
        "deletedAt",
    ] {
        t[field] = Value::Null;
    }
    for field in [
        "hasPendingApprovals",
        "hasPendingUserInput",
        "hasActionableProposedPlan",
    ] {
        t[field] = json!(false);
    }
    for field in ["historyPage", "workspaceKind", "nativeSessionId"] {
        t.as_object_mut().unwrap().remove(field);
    }
    t["id"] = json!(id);
    if spec.is_none() {
        t["title"] = json!("New isolated Claude chat");
    }
    t["branch"] = json!(branch);
    t["worktreePath"] = json!(path);
    t["createdAt"] = json!(time);
    t["updatedAt"] = json!(time);
    if spec.is_none() {
        t["runtimeMode"] = json!("approval-required");
        t["interactionMode"] = json!("default");
    }
    t["session"] = json!({"threadId":id,"providerName":chat.agent.driver(),"providerInstanceId":chat.agent.instance(),"status":"idle","statusConfidence":"confirmed","runtimeMode":t["runtimeMode"],"activeTurnId":null,"lastError":null,"updatedAt":time});
    Ok(Intent {
        repo,
        common,
        head,
        branch,
        path,
        chat,
        scripts: vec![],
        project_root: None,
        setup: SetupStage::Planned,
        existing: false,
    })
}
async fn materialize(base: &Path, p: &Intent) -> Result<(), String> {
    workspace::resolve(base, p.repo.to_str().ok_or("Invalid repository path")?)?;
    let parent = p.path.parent().ok_or("Invalid checkout path")?;
    if workspace::resolve(base, parent.to_str().ok_or("Invalid checkout path")?)? != parent {
        return Err("The checkout parent changed; no files were modified".into());
    }
    let common = workspace::resolve(
        base,
        &text(
            &p.repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )?;
    if common != p.common {
        return Err("The source repository changed; retry with a new request".into());
    }
    let listing = text(&p.repo, &["worktree", "list", "--porcelain", "-z"]).await?;
    let registered = listing
        .split('\0')
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|path| {
            Path::new(path)
                .canonicalize()
                .is_ok_and(|path| path == p.path)
        });
    if !registered {
        if p.existing {
            return Err(
                "The selected worktree is no longer registered; no checkout was created".into(),
            );
        }
        if p.setup != SetupStage::Planned {
            return Err("The prepared checkout is missing. It will not be recreated without its setup files".into());
        }
        // An interrupted checkout or a colliding directory is never deleted.
        if p.path.symlink_metadata().is_ok() {
            return Err(
                "An incomplete checkout needs review on the PC; its files were preserved".into(),
            );
        }
        let reference = format!("refs/heads/{}", p.branch);
        let (code, bytes) = git(&p.repo, &["rev-parse", "--verify", &reference]).await?;
        let path = crate::vcs::path_argument(&p.path)?;
        if code == 0 {
            if String::from_utf8_lossy(&bytes).trim() != p.head {
                return Err("The reserved branch changed; its files were preserved".into());
            }
            text(&p.repo, &["worktree", "add", "--", &path, &p.branch]).await?;
        } else {
            text(
                &p.repo,
                &["worktree", "add", "-b", &p.branch, "--", &path, &p.head],
            )
            .await?;
        }
    }
    let path = workspace::resolve(base, p.path.to_str().ok_or("Invalid checkout path")?)?;
    if path != p.path
        || workspace::resolve(
            base,
            &text(
                &path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?,
        )? != p.common
        || current_branch(&path).await? != p.branch
        || text(&path, &["rev-parse", "HEAD"]).await? != p.head
    {
        return Err("The reserved checkout changed; no files were overwritten".into());
    }
    if !p.existing
        && p.setup == SetupStage::Planned
        && !text(
            &path,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )
        .await?
        .is_empty()
    {
        return Err("The checkout has incomplete or changed files; review it on the PC. All files were preserved".into());
    }
    Ok(())
}

async fn persist(db: &AuthService, key: &str, digest: &str, intent: &Intent) -> Result<(), String> {
    let (key, digest, data) = (
        key.to_owned(),
        digest.to_owned(),
        serde_json::to_string(intent).map_err(|_| "Cannot save setup state")?,
    );
    db.call(move |a| a.store.update_worktree(&key, &digest, &data))
        .await?
        .map_err(|_| "Cannot save setup state; the checkout was preserved".into())
}

async fn setup(
    db: &AuthService,
    key: &str,
    digest: &str,
    intent: &mut Intent,
) -> Result<(), String> {
    match intent.setup {
        SetupStage::Done => return Ok(()),
        SetupStage::Running | SetupStage::Failed => return Err(format!("Setup did not finish successfully. It will not be repeated automatically; review the retained checkout at {}",intent.path.display())),
        SetupStage::Planned => {},
    }
    if intent.scripts.is_empty() {
        intent.setup = SetupStage::Done;
        return persist(db, key, digest, intent).await;
    }
    // Record intent before any non-idempotent command. A crash or failed receipt
    // cannot silently run installation/migration scripts a second time.
    intent.setup = SetupStage::Running;
    persist(db, key, digest, intent).await?;
    let result=tokio::time::timeout(Duration::from_secs(120),async {
        for script in &intent.scripts {
            #[cfg(unix)]
            let mut command={let mut c=tokio::process::Command::new("/bin/sh");c.arg("-c").arg(s(&script["command"]));c};
            #[cfg(windows)]
            let mut command={let mut c=tokio::process::Command::new("cmd.exe");c.arg("/D").arg("/S").arg("/C").arg(s(&script["command"]));c};
            command.current_dir(&intent.path).env("GIT_TERMINAL_PROMPT","0")
                .env("T3CODE_PROJECT_ROOT",intent.project_root.as_ref().unwrap_or(&intent.repo))
                .env("T3CODE_WORKTREE_PATH",&intent.path);
            for key in ["GIT_DIR","GIT_WORK_TREE","GIT_INDEX_FILE","GIT_OBJECT_DIRECTORY","GIT_ALTERNATE_OBJECT_DIRECTORIES"] {command.env_remove(key);}
            let output=crate::subprocess::run(command,Duration::from_secs(120),65536,65536).await?;
            if output.code!=0 {return Err(format!("Setup script {} exited with code {}",s(&script["name"]),output.code));}
            crate::chat_projection::activity(&mut intent.chat,&format!("setup-{}",s(&script["id"])),"project.setup.completed",&format!("Setup completed: {}",s(&script["name"])),"info",json!({"status":"completed","scriptId":script["id"],"command":script["command"],"exitCode":0}));
        }
        if current_branch(&intent.path).await?!=intent.branch {return Err("Setup changed the checkout branch; review it on the PC".into());}
        intent.head=text(&intent.path,&["rev-parse","--verify","HEAD^{commit}"]).await?;
        Ok::<(),String>(())
    }).await.unwrap_or_else(|_|Err("Setup exceeded its two-minute limit".into()));
    intent.setup = if result.is_ok() {
        SetupStage::Done
    } else {
        SetupStage::Failed
    };
    persist(db, key, digest, intent).await?;
    result.map_err(|e|format!("{e}. No chat was started. Review the retained checkout at {}; setup will not repeat automatically",intent.path.display()))
}
pub(crate) async fn prepare(
    db: AuthService,
    base: PathBuf,
    source: Chat,
    key: String,
    digest: String,
    setup_plan: Setup,
    target: Target,
) -> Result<Chat, String> {
    let lookup = key.clone();
    let pending = db
        .call(move |a| a.store.worktree_intent(&lookup))
        .await?
        .map_err(|_| "Worktree store unavailable")?;
    let mut intent = if let Some((old, data)) = pending {
        if old != digest {
            return Err("Command ID was already used for different content".into());
        }
        serde_json::from_str::<Intent>(&data).map_err(|_| "The worktree reservation is invalid")?
    } else {
        let mut intent = match target {
            Target::Create(spec) => plan(&base, source, spec.as_ref()).await?,
            Target::Existing { path, branch } => {
                existing(&base, source, &path, branch.as_deref()).await?
            }
        };
        intent.scripts = setup_plan.scripts;
        intent.project_root = Some(workspace::resolve(
            &base,
            setup_plan
                .project_root
                .to_str()
                .ok_or("Invalid project folder")?,
        )?);
        let (k, d, data) = (
            key.clone(),
            digest.clone(),
            serde_json::to_string(&intent).map_err(|_| "Cannot save worktree reservation")?,
        );
        db.call(move |a| a.store.reserve_worktree(&k, &d, &data))
            .await?
            .map_err(|_| "Cannot reserve worktree; this profile allows 128 owned checkouts")?;
        intent
    };
    if matches!(intent.setup, SetupStage::Running | SetupStage::Failed) {
        return setup(&db, &key, &digest, &mut intent)
            .await
            .and_then(|_| Err("Incomplete setup".into()));
    }
    materialize(&base, &intent).await?;
    setup(&db, &key, &digest, &mut intent).await?;
    Ok(intent.chat)
}
pub(crate) async fn create(
    db: AuthService,
    base: PathBuf,
    source: Chat,
    key: String,
    digest: String,
    setup_plan: Setup,
) -> Result<(Chat, Value), String> {
    let chat = prepare(
        db.clone(),
        base,
        source,
        key.clone(),
        digest.clone(),
        setup_plan,
        Target::Create(None),
    )
    .await?;
    let result = json!({"threadId":chat.thread["id"],"branch":chat.thread["branch"]});
    let state = serde_json::to_value(&chat).map_err(|_| "Cannot save isolated chat")?;
    let value = result.clone();
    db.call(move |a| a.store.save_tool_action(&state, &key, &digest, &value))
        .await?
        .map_err(|_| {
            "Checkout created but chat save failed. Retry this same request to recover it"
        })?;
    Ok((chat, result))
}
