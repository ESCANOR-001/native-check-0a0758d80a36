//! Retryable replacement of one stopped, exactly owned Linux registration.
//! No process is stopped or started, and no database/history snapshot is restored.
use super::*;

fn compatible(value: &Value, existing_profiles: bool) -> bool {
    value["name"] == "remodex-connect-native"
        && value["runtime"] == "rust"
        && value["configurationVersion"] == 1
        && value["storageFormatVersion"] == 1
        && (!existing_profiles || value["existingProfileHandoffVersion"] == 1)
}

async fn check_runtime(plan: &Plan, existing_profiles: bool) -> Result<(), String> {
    verify_binary(plan)?;
    let mut command = tokio::process::Command::new(&plan.executable);
    command.arg("--version");
    let output = crate::subprocess::run(command, Duration::from_secs(10), 8192, 8192).await?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| ERROR)?;
    if output.code != 0 || !compatible(&value, existing_profiles) {
        return Err("The runtime has not declared a compatible native storage format and profile mode; its registration and all chat data were retained".into());
    }
    verify_binary(plan)
}

fn current(path: &Path, old: &[u8], new: &[u8]) -> Result<bool, String> {
    let bytes = profile_snapshot::bytes(path, 16384).map_err(|_| ERROR)?;
    verify_file(path, &bytes)?;
    if bytes == new {
        Ok(true)
    } else if bytes == old {
        Ok(false)
    } else {
        Err(ERROR.into())
    }
}

// Only an exact journal authorizes completing an interrupted private temporary
// write. The temporary and final files must remain regular, single-link files.
fn pending(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if fs::symlink_metadata(path).is_ok() {
        let old = profile_snapshot::bytes(path, 16384).map_err(|_| ERROR)?;
        verify_file(path, &old)?;
        if !bytes.starts_with(&old) {
            return Err(ERROR.into());
        }
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|_| ERROR)?;
    if !file.metadata().map_err(|_| ERROR)?.is_file()
        || storage_stage::links(&file).map_err(|_| ERROR)? != 1
    {
        return Err(ERROR.into());
    }
    file.write_all(bytes).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    Ok(())
}

fn switch_unit(
    root: &Path,
    previous: &Path,
    path: &Path,
    old: &[u8],
    new: &[u8],
) -> Result<(), String> {
    current(path, old, new)?;
    let evidence = serde_json::to_vec(&json!({
        "version":1,"previous":previous,"unit":path,
        "oldSha256":format!("{:x}", Sha256::digest(old)),
        "newSha256":format!("{:x}", Sha256::digest(new)),
    }))
    .map_err(|_| ERROR)?;
    // Keep an immutable receipt per transition. A single receipt per target
    // would incorrectly block A -> B -> C -> B after B previously replaced A.
    let journal = root.join(format!("replacement-{:x}.json", Sha256::digest(&evidence)));
    if fs::symlink_metadata(&journal).is_ok() {
        verify_file(&journal, &evidence)?;
    } else {
        // Durable intent precedes the unit write. Failed/foreign intent is
        // retained for diagnosis, never overwritten by a second operation.
        private_new(&journal, &evidence)?;
    }
    if current(path, old, new)? {
        return Ok(());
    }
    let temporary = path.with_file_name(format!(
        "{}.pending-{:x}",
        path.file_name().and_then(|p| p.to_str()).ok_or(ERROR)?,
        Sha256::digest(&evidence),
    ));
    pending(&temporary, new)?;
    verify_file(&temporary, new)?;
    verify_file(path, old)?;
    fs::rename(&temporary, path).map_err(|_| ERROR)?;
    #[cfg(unix)]
    storage_stage::open(path.parent().ok_or(ERROR)?, true)
        .map_err(|_| ERROR)?
        .sync_all()
        .map_err(|_| ERROR)?;
    verify_file(path, new)
}

pub async fn replace(root: &Path, previous: &Path) -> Result<Value, String> {
    if !cfg!(target_os = "linux") {
        return Err(
            "Native service replacement is not qualified on this platform; no service was changed"
                .into(),
        );
    }
    let root = canonical(root, true)?;
    let previous = canonical(previous, true)?;
    if root == previous || root.starts_with(&previous) || previous.starts_with(&root) {
        return Err(ERROR.into());
    }
    let _target = profile_lock::acquire(&root)?;
    let _previous = profile_lock::acquire(&previous)?;
    let next = read_plan(&root)?;
    let old = read_plan(&previous)?;
    if old.service != next.service {
        return Err(ERROR.into());
    }
    let name = name(&next.service)?;
    let old_bytes = unit(&old)?;
    let new_bytes = unit(&next)?;
    verify_file(&root.join(&name), &new_bytes)?;
    verify_file(&previous.join(&name), &old_bytes)?;
    let units = unit_directory()?;
    let path = units.join(&name);
    current(&path, &old_bytes, &new_bytes)?;
    let state = status(&name).await?;
    own_status(&state, &path)?;
    stopped(&state)?;
    if state.get("FragmentPath").map(String::as_str) != path.to_str() {
        return Err(ERROR.into());
    }
    let wants = units.join("default.target.wants");
    canonical(&wants, true)?;
    if fs::read_link(wants.join(&name)).map_err(|_| ERROR)? != path {
        return Err(ERROR.into());
    }
    let existing_profiles = crate::service_cutover::registration(&next.service)?
        == crate::profile_handoff::ProfileMode::Existing;
    // Exclude another native start/cancel until the replacement is durable.
    let _selected = profile_lock::acquire(&next.service)?;
    check_runtime(&old, existing_profiles).await?;
    check_runtime(&next, existing_profiles).await?;
    stopped(&status(&name).await?)?;
    switch_unit(&root, &previous, &path, &old_bytes, &new_bytes)?;
    if systemctl(&["daemon-reload"]).await?.code != 0 {
        return Err(ERROR.into());
    }
    verify_file(&path, &new_bytes)?;
    let state = status(&name).await?;
    own_status(&state, &path)?;
    stopped(&state)?;
    Ok(
        json!({"phase":"replaced","unit":name,"serviceStarted":false,
        "historyRetained":true,"previousRuntimeRetained":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_probe_is_bounded_and_checks_the_exact_private_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for (script, valid) in [
            ("printf '%s' '{\"name\":\"remodex-connect-native\",\"runtime\":\"rust\",\"configurationVersion\":1,\"storageFormatVersion\":1}'", true),
            ("printf '%s' '{\"name\":\"remodex-connect-native\",\"runtime\":\"rust\",\"configurationVersion\":1}'", false),
            ("printf '%s' 'invalid'", false),
            ("exit 1", false),
            ("head -c 9000 /dev/zero", false),
        ] {
            let executable = dir.path().join("private runtime");
            fs::write(&executable, format!("#!/bin/sh\n[ \"$1\" = --version ] || exit 99\n{script}\n")).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let plan = Plan {
                version: 2, service: dir.path().to_owned(),
                binary_sha256: fingerprint(&executable, None).unwrap(), executable,
            };
            assert_eq!(check_runtime(&plan, false).await.is_ok(), valid);
            assert!(check_runtime(&plan, true).await.is_err());
            fs::write(&plan.executable, b"changed").unwrap();
            assert!(check_runtime(&plan, false).await.is_err());
        }
    }

    #[test]
    fn unknown_and_incompatible_storage_formats_are_not_guessed() {
        let good = json!({"name":"remodex-connect-native","runtime":"rust",
            "configurationVersion":1,"storageFormatVersion":1});
        assert!(compatible(&good, false));
        assert!(!compatible(&good, true));
        let mut existing = good.clone();
        existing["existingProfileHandoffVersion"] = json!(1);
        assert!(compatible(&existing, false));
        assert!(compatible(&existing, true));
        existing["existingProfileHandoffVersion"] = json!(2);
        assert!(!compatible(&existing, true));
        for (key, value) in [
            ("storageFormatVersion", json!(null)),
            ("storageFormatVersion", json!(2)),
            ("configurationVersion", json!(2)),
            ("runtime", json!("other")),
            ("name", json!("another-program")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(!compatible(&bad, false));
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacement_retries_and_reverse_switch_preserve_foreign_files_and_data() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("old");
        let new = temp.path().join("new");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        let unit = temp.path().join("owned.service");
        let history = temp.path().join("history");
        fs::write(&history, b"new work must survive rollback").unwrap();
        fs::write(&unit, b"foreign").unwrap();
        assert!(switch_unit(&new, &old, &unit, b"old", b"new").is_err());
        assert_eq!(fs::read_dir(&new).unwrap().count(), 0);
        fs::write(&unit, b"old").unwrap();
        switch_unit(&new, &old, &unit, b"old", b"new").unwrap();
        switch_unit(&new, &old, &unit, b"old", b"new").unwrap();
        assert_eq!(fs::read(&unit).unwrap(), b"new");
        let journal = fs::read_dir(&new).unwrap().next().unwrap().unwrap().path();
        let evidence = fs::read(&journal).unwrap();
        fs::write(&journal, b"changed intent").unwrap();
        assert!(switch_unit(&new, &old, &unit, b"old", b"new").is_err());
        assert_eq!(fs::read(&unit).unwrap(), b"new");
        fs::write(&journal, evidence).unwrap();
        let third = temp.path().join("third");
        fs::create_dir(&third).unwrap();
        switch_unit(&third, &new, &unit, b"new", b"third").unwrap();
        switch_unit(&new, &third, &unit, b"third", b"new").unwrap();
        switch_unit(&old, &new, &unit, b"new", b"old").unwrap();
        assert_eq!(fs::read(&unit).unwrap(), b"old");
        assert_eq!(
            fs::read(&history).unwrap(),
            b"new work must survive rollback"
        );
        // A later outside modification cannot be overwritten by replaying a
        // completed transition's intent.
        fs::write(&unit, b"outside change").unwrap();
        assert!(switch_unit(&new, &old, &unit, b"old", b"new").is_err());
        assert_eq!(fs::read(&unit).unwrap(), b"outside change");
    }

    #[cfg(unix)]
    #[test]
    fn partial_unit_write_is_retryable_but_changed_and_linked_files_are_not() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending");
        fs::write(&path, b"next").unwrap();
        pending(&path, b"next unit").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"next unit");
        assert!(pending(&path, b"other").is_err());
        let alias = temp.path().join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(pending(&path, b"next unit").is_err());
        fs::remove_file(&alias).unwrap();
        symlink(&path, &alias).unwrap();
        assert!(pending(&alias, b"next unit").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"next unit");
    }
}
