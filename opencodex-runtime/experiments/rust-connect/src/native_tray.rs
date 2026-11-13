//! The Windows notification-area UI controls one explicit native installation.
//! Network requests run on one worker, never on the Windows message thread.
use serde_json::Value;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Refresh,
    Start,
    Stop,
    Restart,
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, Default)]
struct Status {
    // Unknown is deliberately different from stopped. A failed identity check
    // must not enable Start or signal an unrelated listener.
    running: Option<bool>,
    dashboard: Option<String>,
    busy: Option<Action>,
    error: Option<String>,
}
#[cfg(any(windows, test))]
impl Status {
    fn complete(&mut self, action: Action, result: Result<Value, String>) {
        let queued = if action == Action::Refresh && self.busy != Some(Action::Refresh) {
            self.busy
        } else {
            None
        };
        self.error = result.as_ref().err().cloned();
        self.accept(result);
        self.busy = queued;
    }
    fn accept(&mut self, result: Result<Value, String>) {
        self.busy = None;
        match result {
            Ok(value) if value["runtime"] == "rust" && value["running"].is_boolean() => {
                self.running = value["running"].as_bool();
                self.dashboard = value["dashboard"].as_str().and_then(dashboard_url);
            }
            Ok(_) | Err(_) => {
                self.running = None;
                self.dashboard = None;
            }
        }
    }
    fn enabled(&self, action: Action) -> bool {
        (self.busy.is_none() || self.busy == Some(Action::Refresh) && action != Action::Refresh)
            && match action {
                Action::Refresh => true,
                Action::Start => self.running == Some(false),
                Action::Stop | Action::Restart => self.running == Some(true),
            }
    }
    fn label(&self) -> &'static str {
        match self.busy {
            Some(Action::Start) => "Starting Remodex...",
            Some(Action::Stop) => "Stopping Remodex...",
            Some(Action::Restart) => "Restarting Remodex...",
            _ => match self.running {
                Some(true) => "Running - Refresh",
                Some(false) => "Stopped - Refresh",
                None if self.error.is_some() => "Cannot verify server - Refresh",
                None => "Checking connection - Refresh",
            },
        }
    }
}

#[cfg(any(windows, test))]
fn dashboard_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    let address: std::net::IpAddr = url.host_str()?.trim_matches(['[', ']']).parse().ok()?;
    (url.scheme() == "http"
        && address.is_loopback()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.to_string())
}

#[cfg(windows)]
#[path = "native_tray_windows.rs"]
mod platform;

pub fn run(
    operation: &str,
    root: &std::path::Path,
    gui: &std::path::Path,
    controller: Option<&std::path::Path>,
) -> Result<Value, String> {
    #[cfg(windows)]
    {
        platform::run(operation, root, gui, controller)
    }
    #[cfg(not(windows))]
    {
        let _ = (operation, root, gui, controller);
        Err("The native notification-area tray is currently available on Windows only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn background_refresh_does_not_block_or_erase_a_queued_user_action() {
        let mut s = Status {
            running: Some(false),
            busy: Some(Action::Refresh),
            ..Status::default()
        };
        assert!(s.enabled(Action::Start));
        assert!(!s.enabled(Action::Refresh));
        s.busy = Some(Action::Start);
        s.complete(
            Action::Refresh,
            Ok(json!({"runtime":"rust","running":false})),
        );
        assert_eq!(s.busy, Some(Action::Start));
        assert!(!s.enabled(Action::Start));
        s.complete(Action::Start, Ok(json!({"runtime":"rust","running":true})));
        assert_eq!(s.busy, None);
        assert!(s.enabled(Action::Stop));
        s.complete(Action::Refresh, Err("Could not verify this server".into()));
        assert_eq!(s.label(), "Cannot verify server - Refresh");
        assert!(!s.enabled(Action::Start));
    }
    #[test]
    fn unknown_and_busy_states_do_not_offer_unsafe_actions() {
        let mut s = Status::default();
        assert!(s.enabled(Action::Refresh));
        assert!(!s.enabled(Action::Start));
        s.accept(Ok(
            json!({"runtime":"rust","running":false,"dashboard":"http://127.0.0.1:1234"}),
        ));
        assert!(s.enabled(Action::Start));
        assert!(!s.enabled(Action::Stop));
        s.accept(Ok(
            json!({"runtime":"rust","running":true,"dashboard":"http://[::1]:1234"}),
        ));
        assert!(s.enabled(Action::Stop));
        assert!(s.enabled(Action::Restart));
        assert!(!s.enabled(Action::Start));
        s.busy = Some(Action::Restart);
        assert!(!s.enabled(Action::Restart));
        assert!(!s.enabled(Action::Refresh));
        assert_eq!(s.label(), "Restarting Remodex...");
        s.accept(Err("identity mismatch".into()));
        assert!(!s.enabled(Action::Start));
        assert!(s.dashboard.is_none());
        s.accept(Ok(json!({"running":false})));
        assert!(!s.enabled(Action::Start));
    }
    #[test]
    fn dashboard_links_cannot_be_commands_or_remote_destinations() {
        assert_eq!(
            dashboard_url("http://127.0.0.1:10100").unwrap(),
            "http://127.0.0.1:10100/"
        );
        for value in [
            "https://example.com",
            "file:///C:/Windows",
            "http://127.0.0.1@evil.test",
            "http://user@127.0.0.1",
            "http://127.0.0.1/path",
            "http://127.0.0.1/?token=secret",
            "http://127.0.0.1/#fragment",
            "http://localhost",
            "http://192.168.1.2",
        ] {
            assert!(dashboard_url(value).is_none(), "{value}");
        }
    }
}
