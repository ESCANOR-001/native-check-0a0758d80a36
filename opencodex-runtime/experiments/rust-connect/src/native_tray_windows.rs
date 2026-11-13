use super::{Action, Status};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    fs::{self, OpenOptions},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
#[cfg(feature = "tray-test-controls")]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, RemoteDesktop::ProcessIdToSessionId, Threading::*},
    UI::{Shell::*, WindowsAndMessaging::*},
};

const CALLBACK: u32 = WM_APP + 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
#[cfg(feature = "tray-test-controls")]
const SHOW: u32 = WM_APP + 2;
const QUIT: u32 = WM_APP + 3;
const REFRESH: usize = 101;
const DASHBOARD: usize = 102;
const START: usize = 103;
const STOP: usize = 104;
const RESTART: usize = 105;
const EXIT: usize = 107;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn key(root: &Path) -> Result<String, String> {
    let root = fs::canonicalize(root).map_err(|_| "The selected installation folder is missing")?;
    Ok(format!(
        "Remodex.NativeTray.{:x}",
        Sha256::digest(root.to_string_lossy().to_lowercase().as_bytes())
    ))
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(super) fn run(
    operation: &str,
    root: &Path,
    gui: &Path,
    controller: Option<&Path>,
) -> Result<Value, String> {
    if !(matches!(operation, "start" | "status" | "quit" | "host")
        || cfg!(feature = "tray-test-controls") && matches!(operation, "show" | "host-show"))
    {
        return Err("Usage: rmx tray [status|quit]".into());
    }
    if crate::native_cli::installation_kind(root)? == crate::native_cli::InstallationKind::New {
        return Err("Set up Remodex with rmx onboard before opening its tray".into());
    }
    let root = fs::canonicalize(root).map_err(|_| "Cannot read the selected installation")?;
    let class = wide(&key(&root)?);
    let existing = unsafe { FindWindowW(class.as_ptr(), null()) };
    if operation == "status" {
        return Ok(json!({"trayRunning": !existing.is_null()}));
    }
    if operation == "quit" {
        if !existing.is_null() {
            unsafe {
                PostMessageW(existing, QUIT, 0, 0);
            }
        }
        return Ok(json!({"trayQuitRequested": !existing.is_null(), "serverKeptRunning":true}));
    }
    if !existing.is_null() {
        #[cfg(feature = "tray-test-controls")]
        if matches!(operation, "show" | "host-show") {
            unsafe {
                PostMessageW(existing, SHOW, 0, 0);
            }
        }
        return Ok(json!({"trayRunning":true,"alreadyRunning":true}));
    }
    if operation.starts_with("host") {
        let controller = controller.ok_or("The tray controller path is missing")?;
        let result = host(
            root.clone(),
            gui.to_owned(),
            controller.to_owned(),
            class,
            operation == "host-show",
        );
        if let Err(error) = &result {
            use std::io::Write;
            if let Ok(mut log) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("native-tray.log"))
            {
                let _ = writeln!(log, "{error}");
            }
        }
        return result;
    }
    let controller = std::env::current_exe().map_err(|_| "Cannot locate Remodex")?;
    let image = stage(&root, &controller)?;
    // A separate windowless process outlives the command that opened the tray.
    // Its worker never owns a kill-on-close job or kills the server on exit.
    let text = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or("Cannot represent the tray path")
    };
    let arguments = [
        text(&image)?,
        "--native-tray".into(),
        if operation == "show" {
            "host-show".into()
        } else {
            "host".into()
        },
        text(&root)?,
        text(gui)?,
        text(&controller)?,
    ];
    let mut line = wide(
        &arguments
            .iter()
            .map(|s| crate::native_service::windows_argument(s))
            .collect::<Result<Vec<_>, _>>()?
            .join(" "),
    );
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // A long-lived tray must not inherit the invoking terminal's pipes or any
    // listening sockets. In particular, stdio redirection alone does not close
    // every inheritable handle supplied by a Node/Bun caller on Windows.
    if unsafe {
        CreateProcessW(
            wide(&text(&image)?).as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            null(),
            null(),
            &startup,
            &mut process,
        )
    } == 0
    {
        return Err("Cannot start the native tray".into());
    }
    let child = Handle(process.hProcess);
    let _thread = Handle(process.hThread);
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if !unsafe { FindWindowW(class.as_ptr(), null()) }.is_null() {
            return Ok(
                json!({"trayRunning":true,"pid":process.dwProcessId,"serverKeptRunning":true}),
            );
        }
        if unsafe { WaitForSingleObject(child.0, 0) } == WAIT_OBJECT_0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("The tray did not become ready. See native-tray.log in the selected installation".into())
}

fn stage(root: &Path, source: &Path) -> Result<PathBuf, String> {
    let directory = root.join("windows-tray");
    crate::private_directory::prepare(&directory).map_err(|_| "Cannot prepare the tray folder")?;
    let _lease = crate::profile_lock::acquire(&directory)?;
    let expected = crate::service_registration::fingerprint(source, None)?;
    let image = directory.join(format!("tray-{expected}.exe"));
    if !image
        .try_exists()
        .map_err(|_| "Cannot inspect the tray image")?
    {
        let temporary =
            tempfile::tempdir_in(&directory).map_err(|_| "Cannot stage the tray image")?;
        let file = temporary.path().join("tray.exe");
        if crate::service_registration::fingerprint(source, Some(&file))? != expected {
            return Err("The Remodex build changed while preparing the tray".into());
        }
        fs::rename(file, &image).map_err(|_| "Cannot save the tray image")?;
    }
    let file =
        crate::storage_stage::open(&image, false).map_err(|_| "Cannot verify the tray image")?;
    if crate::storage_stage::links(&file).map_err(|_| "Cannot verify the tray image")? != 1
        || crate::service_registration::fingerprint(&image, None)? != expected
    {
        return Err("The saved tray image has changed; no program was started".into());
    }
    Ok(image)
}

fn start_server(root: &Path, gui: &Path, controller: &Path) -> Result<(), String> {
    crate::native_package::verify_if_packaged(controller)?;
    let args = crate::native_cli::translate(&["start".into()], root, gui, root, root)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("tray-server.log"))
        .map_err(|_| "Cannot open the server log")?;
    let mut child = Command::new(controller)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|_| "Cannot open server log")?)
        .stderr(log)
        .spawn()
        .map_err(|_| "Cannot start the selected server")?;
    // Reap the child when it eventually stops. Dropping a Child never kills it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn worker(
    root: PathBuf,
    gui: PathBuf,
    controller: PathBuf,
    status: Arc<Mutex<Status>>,
    rx: mpsc::Receiver<Action>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => {
            status.lock().unwrap().error = Some("Cannot start the status checker".into());
            return;
        }
    };
    loop {
        let action = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(action) => action,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut s = status.lock().unwrap();
                if s.busy.is_some() {
                    continue;
                }
                s.busy = Some(Action::Refresh);
                Action::Refresh
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let result = runtime.block_on(async {
            match action {
                Action::Refresh => {},
                Action::Start => {
                    // Recheck immediately before starting: a stale menu is not authority.
                    let before = crate::npm_upgrade::command(&root, "status").await?;
                    if before["running"] != false { return Err("The server is already running; refresh its status".into()); }
                    start_server(&root, &gui, &controller)?;
                    let until = Instant::now() + Duration::from_secs(60);
                    loop {
                        if let Ok(next) = crate::npm_upgrade::command(&root, "status").await {
                            if next["running"] == true { return Ok(next); }
                        }
                        if Instant::now() >= until { return Err("Startup was not confirmed. See tray-server.log; no process was force-stopped".into()); }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                },
                Action::Stop | Action::Restart => {
                    crate::npm_upgrade::command(&root, if action == Action::Stop {"stop"} else {"restart"}).await?;
                },
            }
            crate::npm_upgrade::command(&root, "status").await
        });
        let mut s = status.lock().unwrap();
        s.complete(action, result);
    }
}

#[derive(Clone)]
struct Ui {
    hwnd: HWND,
    #[cfg(feature = "tray-test-controls")]
    label: HWND,
    #[cfg(feature = "tray-test-controls")]
    buttons: Vec<(usize, HWND)>,
    icon: HICON,
    taskbar_created: u32,
    status: Arc<Mutex<Status>>,
    tx: mpsc::SyncSender<Action>,
    added: bool,
    last_error: Option<String>,
    last_tip: String,
}
thread_local! { static UI: RefCell<Option<Ui>> = const { RefCell::new(None) }; }
fn ui() -> Option<Ui> {
    UI.with(|u| u.borrow().clone())
}
fn request(u: &Ui, action: Action) {
    let mut s = u.status.lock().unwrap();
    if s.enabled(action) {
        let previous = s.busy;
        s.busy = Some(action);
        if u.tx.try_send(action).is_err() {
            s.busy = previous;
        }
    }
}
unsafe fn notification(u: &Ui, operation: u32) -> bool {
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = u.hwnd;
    data.uID = 1;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = CALLBACK;
    data.hIcon = u.icon;
    let status = u.status.lock().unwrap().label();
    let tip = wide(&format!("Remodex - {status}"));
    for (target, value) in data.szTip.iter_mut().zip(tip) {
        *target = value;
    }
    let ok = Shell_NotifyIconW(operation, &data) != 0;
    if operation == NIM_ADD && ok {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
    ok
}
unsafe fn render(u: &Ui) {
    let s = u.status.lock().unwrap().clone();
    #[cfg(feature = "tray-test-controls")]
    {
        let text = if let Some(error) = &s.error {
            format!("{}\r\n{error}", s.label())
        } else {
            s.label().into()
        };
        SetWindowTextW(u.label, wide(&text).as_ptr());
        for &(id, hwnd) in &u.buttons {
            let enabled = match id {
                REFRESH => s.enabled(Action::Refresh),
                START => s.enabled(Action::Start),
                STOP => s.enabled(Action::Stop),
                RESTART => s.enabled(Action::Restart),
                DASHBOARD => s.dashboard.is_some() && s.running == Some(true),
                EXIT => !matches!(s.busy, Some(Action::Start | Action::Stop | Action::Restart)),
                _ => true,
            };
            EnableWindow(hwnd, i32::from(enabled));
        }
    }
    let added = if u.added {
        if u.last_tip == s.label() {
            true
        } else {
            notification(u, NIM_MODIFY)
        }
    } else {
        notification(u, NIM_ADD) || notification(u, NIM_MODIFY)
    };
    if s.error != u.last_error {
        if let Some(error) = &s.error {
            let mut data: NOTIFYICONDATAW = std::mem::zeroed();
            data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = u.hwnd;
            data.uID = 1;
            data.uFlags = NIF_INFO;
            data.dwInfoFlags = NIIF_ERROR;
            for (out, value) in data.szInfoTitle.iter_mut().zip(wide("Remodex")) {
                *out = value;
            }
            for (out, value) in data.szInfo.iter_mut().take(255).zip(error.encode_utf16()) {
                *out = value;
            }
            Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
    {
        UI.with(|v| {
            if let Some(u) = v.borrow_mut().as_mut() {
                u.added = added;
                u.last_tip = s.label().into();
                u.last_error = s.error;
            }
        });
    }
}
#[cfg(feature = "tray-test-controls")]
unsafe fn show(u: &Ui) {
    ShowWindow(u.hwnd, SW_SHOWNORMAL);
    SetForegroundWindow(u.hwnd);
    request(u, Action::Refresh);
    render(u);
}
unsafe fn action(u: &Ui, id: usize) {
    let can_quit = !matches!(
        u.status.lock().unwrap().busy,
        Some(Action::Start | Action::Stop | Action::Restart)
    );
    match id {
        REFRESH => request(u, Action::Refresh),
        START => request(u, Action::Start),
        STOP => request(u, Action::Stop),
        RESTART => request(u, Action::Restart),
        DASHBOARD => {
            let s = u.status.lock().unwrap().clone();
            if let Some(url) = s.dashboard.filter(|_| s.running == Some(true)) {
                let result = ShellExecuteW(
                    u.hwnd,
                    wide("open").as_ptr(),
                    wide(&url).as_ptr(),
                    null(),
                    null(),
                    SW_SHOWNORMAL,
                );
                if result as isize <= 32 {
                    u.status.lock().unwrap().error =
                        Some("Windows could not open your default browser".into());
                }
            }
        }
        EXIT if can_quit => {
            DestroyWindow(u.hwnd);
            return;
        }
        _ => {}
    }
    render(u);
}
unsafe fn menu(u: &Ui) {
    request(u, Action::Refresh);
    let popup = CreatePopupMenu();
    if popup.is_null() {
        return;
    }
    let s = u.status.lock().unwrap().clone();
    for (id, text, enabled) in [
        (REFRESH, s.label(), s.enabled(Action::Refresh)),
        (
            DASHBOARD,
            "Open dashboard",
            s.running == Some(true) && s.dashboard.is_some(),
        ),
        (START, "Start server", s.enabled(Action::Start)),
        (STOP, "Stop server", s.enabled(Action::Stop)),
        (RESTART, "Restart Remodex", s.enabled(Action::Restart)),
        (
            EXIT,
            "Quit tray (keep server running)",
            !matches!(s.busy, Some(Action::Start | Action::Stop | Action::Restart)),
        ),
    ] {
        if matches!(id, START | EXIT) {
            AppendMenuW(popup, MF_SEPARATOR, 0, null());
        }
        AppendMenuW(
            popup,
            MF_STRING | if enabled { MF_ENABLED } else { MF_GRAYED },
            id,
            wide(text).as_ptr(),
        );
    }
    let mut at: POINT = std::mem::zeroed();
    GetCursorPos(&mut at);
    SetForegroundWindow(u.hwnd);
    let chosen = TrackPopupMenuEx(
        popup,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        at.x,
        at.y,
        u.hwnd,
        null(),
    ) as usize;
    DestroyMenu(popup);
    PostMessageW(u.hwnd, WM_NULL, 0, 0);
    if chosen != 0 {
        action(u, chosen);
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // No RefCell or mutex guard is held over Windows calls: popup menus run a
    // nested message loop and can re-enter this procedure.
    if let Some(u) = ui() {
        if message == u.taskbar_created {
            UI.with(|v| {
                if let Some(u) = v.borrow_mut().as_mut() {
                    u.added = false;
                }
            });
            render(&ui().unwrap());
            return 0;
        }
        match message {
            CALLBACK => {
                match (l as u32) & 0xffff {
                    WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => menu(&u),
                    _ => {}
                }
                return 0;
            }
            #[cfg(feature = "tray-test-controls")]
            SHOW => {
                show(&u);
                return 0;
            }
            QUIT => {
                action(&u, EXIT);
                return 0;
            }
            WM_COMMAND => {
                action(&u, w & 0xffff);
                return 0;
            }
            WM_CONTEXTMENU => {
                menu(&u);
                return 0;
            }
            WM_TIMER => {
                render(&u);
                return 0;
            }
            WM_CLOSE => {
                #[cfg(feature = "tray-test-controls")]
                ShowWindow(hwnd, SW_HIDE);
                return 0;
            }
            WM_DESTROY => {
                KillTimer(hwnd, 1);
                notification(&u, NIM_DELETE);
                UI.with(|v| {
                    v.borrow_mut().take();
                });
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, w, l)
}

fn host(
    root: PathBuf,
    gui: PathBuf,
    controller: PathBuf,
    class: Vec<u16>,
    visible: bool,
) -> Result<Value, String> {
    // Session-local ownership avoids duplicate icons without assuming a PID is
    // trustworthy. Services in session 0 must not create an invisible UI.
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 || session == 0 {
        return Err(
            "Open the tray from your signed-in Windows desktop, not a system service".into(),
        );
    }
    let name = wide(&format!("Local\\{}", key(&root)?));
    let mutex = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if mutex.is_null() {
        return Err("Cannot reserve the tray for this installation".into());
    }
    let _owner = Handle(mutex);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(json!({"alreadyRunning":true}));
    }
    let status = Arc::new(Mutex::new(Status::default()));
    let (tx, rx) = mpsc::sync_channel(1);
    unsafe {
        let instance = GetModuleHandleW(null());
        let icon = LoadImageW(
            instance,
            101usize as *const u16,
            IMAGE_ICON,
            32,
            32,
            LR_SHARED,
        ) as HICON;
        if icon.is_null() {
            return Err("The Remodex tray icon is missing from this build".into());
        }
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            hIcon: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (windows_sys::Win32::Graphics::Gdi::COLOR_WINDOW + 1) as _,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("Cannot create the Remodex tray window".into());
        }
        #[cfg(feature = "tray-test-controls")]
        let label = root
            .parent()
            .and_then(Path::file_name)
            .unwrap_or_default()
            .to_string_lossy();
        #[cfg(feature = "tray-test-controls")]
        let title = wide(&format!("Remodex test controls - {label}"));
        #[cfg(not(feature = "tray-test-controls"))]
        let title = wide("Remodex tray");
        let style = if cfg!(feature = "tray-test-controls") {
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX
        } else {
            WS_POPUP
        };
        let hwnd = CreateWindowExW(
            if cfg!(feature = "tray-test-controls") {
                0
            } else {
                WS_EX_TOOLWINDOW
            },
            class.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            590,
            290,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err("Cannot create the Remodex tray".into());
        }
        #[cfg(feature = "tray-test-controls")]
        let (text, buttons) = {
            let text = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("Checking connection...").as_ptr(),
                WS_CHILD | WS_VISIBLE,
                20,
                20,
                540,
                80,
                hwnd,
                null_mut(),
                instance,
                null(),
            );
            let mut buttons = Vec::new();
            for (index, (id, caption)) in [
                (REFRESH, "Refresh"),
                (DASHBOARD, "Open dashboard"),
                (START, "Start server"),
                (STOP, "Stop server"),
                (RESTART, "Restart Remodex"),
                (EXIT, "Quit tray"),
            ]
            .iter()
            .enumerate()
            {
                let button = CreateWindowExW(
                    0,
                    wide("BUTTON").as_ptr(),
                    wide(caption).as_ptr(),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    20 + (index % 3) as i32 * 180,
                    110 + (index / 3) as i32 * 48,
                    170,
                    36,
                    hwnd,
                    *id as HMENU,
                    instance,
                    null(),
                );
                buttons.push((*id, button));
            }
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("Closing this window or quitting the tray keeps the server running.").as_ptr(),
                WS_CHILD | WS_VISIBLE,
                20,
                214,
                550,
                24,
                hwnd,
                null_mut(),
                instance,
                null(),
            );
            let font = windows_sys::Win32::Graphics::Gdi::GetStockObject(
                windows_sys::Win32::Graphics::Gdi::DEFAULT_GUI_FONT,
            );
            SendMessageW(text, WM_SETFONT, font as usize, 1);
            for &(_, button) in &buttons {
                SendMessageW(button, WM_SETFONT, font as usize, 1);
            }
            (text, buttons)
        };
        let u = Ui {
            hwnd,
            #[cfg(feature = "tray-test-controls")]
            label: text,
            #[cfg(feature = "tray-test-controls")]
            buttons,
            icon,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            status: status.clone(),
            tx,
            added: false,
            last_error: None,
            last_tip: String::new(),
        };
        UI.with(|v| {
            *v.borrow_mut() = Some(u.clone());
        });
        std::thread::spawn(move || worker(root, gui, controller, status, rx));
        request(&u, Action::Refresh);
        SetTimer(hwnd, 1, 1000, None);
        render(&u);
        #[cfg(feature = "tray-test-controls")]
        if visible {
            show(&u);
        }
        #[cfg(not(feature = "tray-test-controls"))]
        let _ = visible;
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let result = GetMessageW(&mut msg, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            #[cfg(feature = "tray-test-controls")]
            if IsDialogMessageW(hwnd, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(json!({"trayClosed":true,"serverKeptRunning":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staged_tray_is_separate_from_replaceable_package_and_rejects_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profile space 测试");
        fs::create_dir(&root).unwrap();
        let source = temp.path().join("rmx.exe");
        fs::write(&source, b"first trusted runtime").unwrap();
        fs::write(root.join("saved-chats.fixture"), b"retained").unwrap();
        let first = stage(&root, &source).unwrap();
        assert_ne!(first, source);
        assert_eq!(stage(&root, &source).unwrap(), first);
        fs::write(&source, b"updated trusted runtime").unwrap();
        let second = stage(&root, &source).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first trusted runtime");
        fs::write(&second, b"changed after staging").unwrap();
        assert!(stage(&root, &source).is_err());
        assert_eq!(
            fs::read(root.join("saved-chats.fixture")).unwrap(),
            b"retained"
        );
    }
    #[test]
    fn different_installations_have_distinct_single_instance_keys() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("one");
        let second = temp.path().join("two");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        assert_ne!(key(&first).unwrap(), key(&second).unwrap());
        assert_eq!(
            key(&first).unwrap(),
            key(&fs::canonicalize(&first).unwrap()).unwrap()
        );
        assert_eq!(
            key(&first).unwrap(),
            key(&PathBuf::from(first.to_string_lossy().to_uppercase())).unwrap()
        );
    }
}
