//! Opt-in real SCM check. It needs administrator approval and refuses to run
//! when either historical Remodex service name already exists.
use super::*;
use std::{os::windows::process::CommandExt, time::Instant};

#[test]
fn recorded_custom_profile_is_used_and_unapproved_environment_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap();
    let installation = source.join("native-installation");
    let profile = source.join("custom Codex profile");
    fs::create_dir(&installation).unwrap();
    fs::create_dir(&profile).unwrap();
    let sid = windows_task::current_sid().unwrap();
    let mut r = Receipt {
        version: 1,
        installation: installation.clone(),
        name: NAMES[0].into(),
        sid: sid.clone(),
        original: Configuration {
            kind: SERVICE_WIN32_OWN_PROCESS,
            start: SERVICE_DEMAND_START,
            error: SERVICE_ERROR_NORMAL,
            binary: "old.exe".into(),
            account: sid,
            group: String::new(),
            dependencies: Vec::new(),
            display: "fixture".into(),
        },
        xml_sha256: String::new(),
        environment: BTreeMap::from([("CODEX_HOME".into(), profile.to_str().unwrap().into())]),
    };
    let saved = receipt_path(&installation).unwrap();
    fs::write(&saved, serde_json::to_vec(&r).unwrap()).unwrap();
    assert_eq!(codex_profile(&installation).unwrap(), Some(profile));
    r.environment.insert(
        "OPENCODEX_ADMIN_AUTH_TOKEN".into(),
        "must-not-import".into(),
    );
    fs::write(&saved, serde_json::to_vec(&r).unwrap()).unwrap();
    assert!(codex_profile(&installation).is_err());
    r.environment.remove("OPENCODEX_ADMIN_AUTH_TOKEN");
    r.original.account = "S-1-5-18".into();
    fs::write(&saved, serde_json::to_vec(&r).unwrap()).unwrap();
    assert!(
        codex_profile(&installation).is_err(),
        "SYSTEM service cannot be adopted"
    );
}

struct Cleanup {
    service: Handle,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut status = unsafe { std::mem::zeroed() };
        unsafe {
            ControlService(self.service.0, SERVICE_CONTROL_STOP, &mut status);
        }
        let until = Instant::now() + Duration::from_secs(40);
        while state(&self.service).ok() != Some(SERVICE_STOPPED) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe {
            DeleteService(self.service.0);
        }
    }
}
#[test]
#[ignore = "creates one isolated LocalService SCM fixture; explicit administrator approval required"]
fn real_service_preserves_account_and_closes_child_cleanly() {
    let output =
        PathBuf::from(std::env::var_os("REMODEX_SCM_CHECK_DIR").expect("explicit test directory"));
    assert!(output.is_absolute());
    assert_eq!(
        output.parent().unwrap().file_name().unwrap(),
        "RemodexUpgradeChecks"
    );
    assert!(!output.exists(), "fixture directory must be new");
    for name in NAMES {
        assert!(
            open(name, SERVICE_QUERY_CONFIG).unwrap().is_none(),
            "existing service must not be touched"
        );
    }
    fs::create_dir(&output).unwrap();
    let test = std::panic::catch_unwind(|| {
        let binaries = PathBuf::from(std::env::var_os("REMODEX_SCM_BIN_DIR").unwrap());
        fs::copy(
            binaries.join("remodex-startup.exe"),
            output.join("remodex-startup.exe"),
        )
        .unwrap();
        fs::copy(binaries.join("scm-fixture.exe"), output.join("runtime.exe")).unwrap();
        fs::write(
            output.join("scm-fixture.marker"),
            b"isolated-remodex-service-test-v1",
        )
        .unwrap();
        // Only this newly-created disposable directory is writable by the
        // built-in LocalService account. No actual profile is shared.
        let grant = std::process::Command::new("C:\\Windows\\System32\\icacls.exe")
            .arg(&output)
            .args(["/grant", "*S-1-5-19:(OI)(CI)(M)"])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(grant.status.success(), "fixture access failed");
        let manager =
            Handle(unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CREATE_SERVICE) });
        assert!(!manager.0.is_null(), "administrator approval required");
        let old_path =
            windows_argument(&windows_task::dos_path(&output.join("old-not-started.exe")).unwrap())
                .unwrap();
        let raw = unsafe {
            CreateServiceW(
                manager.0,
                wide(NAMES[0]).unwrap().as_ptr(),
                wide("Remodex isolated upgrade check").unwrap().as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_DEMAND_START,
                SERVICE_ERROR_NORMAL,
                wide(&old_path).unwrap().as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                wide("NT AUTHORITY\\LocalService").unwrap().as_ptr(),
                ptr::null(),
            )
        };
        assert!(!raw.is_null(), "{}", failure());
        let cleanup = Cleanup {
            service: Handle(raw),
        };
        let before = configuration(&cleanup.service).unwrap();
        assert_eq!(before.account, "NT AUTHORITY\\LocalService");
        // The production importer must refuse this fixture account, even when
        // its service name matches. Real migrations require the owning user.
        assert!(validate_legacy(&output, NAMES[0], &before, &json!({})).is_err());
        let next = command(&output, NAMES[0]).unwrap();
        let mut wrong = before.clone();
        wrong.binary = "unrelated-service.exe".into();
        assert!(mutation_handle(NAMES[0], SERVICE_CHANGE_CONFIG, &wrong).is_err());
        assert_eq!(configuration(&cleanup.service).unwrap(), before);
        let writable = mutation_handle(NAMES[0], SERVICE_CHANGE_CONFIG, &before).unwrap();
        change(&writable, &next).unwrap();
        drop(writable);
        let after = configuration(&cleanup.service).unwrap();
        let mut recovered = after.clone();
        recovered.binary = before.binary.clone();
        assert_eq!(
            recovered, before,
            "account/start/dependencies must be unchanged"
        );
        assert_eq!(after.binary, next);
        assert_ne!(
            unsafe { StartServiceW(cleanup.service.0, 0, ptr::null()) },
            0,
            "{}",
            failure()
        );
        let until = Instant::now() + Duration::from_secs(20);
        while !output.join("child-started").exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(fs::read(output.join("child-started")).unwrap(), b"ready");
        assert_eq!(state(&cleanup.service).unwrap(), SERVICE_RUNNING);
        let mut status = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe { ControlService(cleanup.service.0, SERVICE_CONTROL_STOP, &mut status) },
            0
        );
        let until = Instant::now() + Duration::from_secs(40);
        while state(&cleanup.service).unwrap() != SERVICE_STOPPED && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(state(&cleanup.service).unwrap(), SERVICE_STOPPED);
        assert_eq!(
            fs::read(output.join("child-graceful-exit")).unwrap(),
            b"closed"
        );
        // Reverting the command also keeps credentials and startup policy.
        change(&cleanup.service, &old_path).unwrap();
        assert_eq!(configuration(&cleanup.service).unwrap(), before);
        drop(cleanup);
    });
    let passed = test.is_ok();
    fs::write(output.join("result.json"), json!({"passed":passed,"serviceRemoved":open(NAMES[0], SERVICE_QUERY_CONFIG).is_ok_and(|s|s.is_none()),"realProfilesUsed":false}).to_string()).unwrap();
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}
