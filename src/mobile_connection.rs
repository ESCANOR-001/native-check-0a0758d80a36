//! Existing Android connection-update contract; one bounded snapshot per bridge.
use serde_json::{json, Value};

pub struct State {
    instance: String,
    revision: u64,
    current: Value,
}
impl Default for State {
    fn default() -> Self {
        Self {
            instance: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            current: Value::Null,
        }
    }
}
impl State {
    pub fn snapshot(&mut self, local: Vec<String>, tunnel: Option<crate::tunnel::State>) -> Value {
        let cloudflare = match tunnel {
            Some(s) if s.status == "ready" && s.public_url.is_some() => {
                json!({"status":"ready","url":s.public_url})
            }
            Some(s) if s.status == "checking" => json!({"status":"checking"}),
            Some(s) if s.status == "starting" => json!({"status":"starting"}),
            Some(s) => {
                json!({"status":"unavailable","reason":s.error.unwrap_or("backend_unavailable")})
            }
            None => json!({"status":"unavailable","reason":"backend_unavailable"}),
        };
        if self.current["localUrls"] != json!(local) || self.current["cloudflare"] != cloudflare {
            self.revision += 1;
            self.current = json!({"version":1,"desktopInstanceId":self.instance,"revision":self.revision,
                "localUrls":local,"cloudflare":cloudflare,"updatedAt":crate::auth::timestamp(crate::auth::now())});
        }
        self.current.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revisions_change_only_with_routes_and_restart_gets_a_new_instance() {
        let mut first = State::default();
        let old = first.snapshot(vec!["http://127.0.0.1:18000".into()], None);
        for _ in 0..1000 {
            assert_eq!(
                first.snapshot(vec!["http://127.0.0.1:18000".into()], None),
                old
            );
        }
        let disabled = first.snapshot(vec![], None);
        assert!(disabled["revision"].as_u64() > old["revision"].as_u64());
        assert_eq!(disabled["desktopInstanceId"], old["desktopInstanceId"]);
        let restart = State::default().snapshot(vec![], None);
        assert_ne!(restart["desktopInstanceId"], old["desktopInstanceId"]);
    }
}
