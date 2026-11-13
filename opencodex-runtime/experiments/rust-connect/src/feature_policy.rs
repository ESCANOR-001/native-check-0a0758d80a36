//! Product boundary for native Connect. This is not a claim that the public
//! provider-proxy library has been reimplemented in Rust.
use serde_json::{json, Value};
pub fn catalog() -> Value {
    json!({"version":1,"product":"native-connect","migrationComplete":false,
      "native":["codex-chats","claude-chats","history","pairing","uploads","one-shot-commands","dashboard","quick-tunnel","named-tunnel","startup-configuration","background-startup","command-launcher","package-builder","verified-legacy-import"],
      "nativeDistribution":{"nodeRequired":false,"bunRequired":false,"legacySource":"Verified v1 handoff from the old owner, or a previously closed export"},
      "retiredFromNativeConnect":[
        {"feature":"persistent-terminal","replacement":"Use the agent's tools or the PC terminal; native one-shot commands do not provide a persistent interactive shell"},
        {"feature":"background-preview-server","replacement":"Start and manage preview servers in the project on the PC"}
      ],
      "platformFeatures":{"windows":["native-tray"]},
      "separateProducts":["provider-proxy","public-bun-import-api","standalone-relay"],
      "retainedCompatibility":["legacy-upgrade-exporter","legacy-service-handoff"],
      "deletionAllowed":false})
}
pub fn retired(command: &str) -> Option<&'static str> {
    match command{
    "__tray-host"|"__tray-start"|"__tray-restart"=>Some("Use rmx tray for the native Windows tray. The old tray's private commands are retired. No legacy backend was started."),
    "terminal"=>Some("Persistent interactive terminals are retired from native Connect. Use the PC terminal or agent tools. Native one-shot execution is still available."),
    "preview"=>Some("Background preview-server management is retired from native Connect. Start and manage the preview in the project on the PC."),
    _=>None}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_retired_feature_is_advertised_as_native() {
        let c = catalog();
        for row in c["retiredFromNativeConnect"].as_array().unwrap() {
            assert!(!c["native"].as_array().unwrap().contains(&row["feature"]));
        }
        assert_eq!(c["deletionAllowed"], false);
        assert!(retired("tray").is_none());
        assert!(retired("__tray-host")
            .unwrap()
            .contains("No legacy backend"));
        assert!(retired("start").is_none());
    }
}
