//! Report live status of a mooR server's web host: health, version, and
//! feature flags over its HTTP API.

wit_bindgen::generate!({ world: "tool", path: "../../wit", generate_all });
mod moo;
use moo::{bounded, features, health, version, Health, Moo};
use serde_json::json;
use thetis::grip::types::LogLevel;
struct Component;
impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "moo-server-info".into(),
            description: "Report live status of a mooR server's web host: health, version, and feature flags over its HTTP API. Reads /health, /version and /v1/features, none of which need authentication. The server address comes only from [tools.moo] base_url in config (default http://10.10.10.1:7892) — there is no argument to point this at a different server.".into(),
            args_schema_json: json!({"type":"object","properties":{},"additionalProperties":false}).to_string(),
            capabilities: vec!["group:moo".into(), "http".into(), "read-only".into()],
        }
    }
    fn invoke(_: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(&args_json).map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        if args.as_object().is_none_or(|m| !m.is_empty()) { return Err("moo-server-info accepts no arguments".into()); }
        let client = Moo::from_config(&config_json)?;
        thetis::grip::sys::log(LogLevel::Debug, &format!("moo-server-info: querying {}", client.base_url));
        let mut out = format!("mooR web host at {}\n", client.base_url);
        match health(&client) {
            Ok(Health::Healthy) => out.push_str("health: healthy\n"),
            Ok(Health::Unhealthy) => out.push_str("health: UNHEALTHY (web host cannot hear daemon)\n"),
            Err(e) => return Err(format!("could not check {}/health: {e}", client.base_url)),
        }
        match version(&client) { Ok(v) => out.push_str(&format!("version: {} (commit {})\n", v.version, v.commit)), Err(e) => out.push_str(&format!("version: could not read ({e})\n")) }
        match features(&client) {
            Ok(flags) => {
                out.push_str("features:\n");
                if let Some(map) = flags.as_object() {
                    let mut names: Vec<_> = map.keys().collect(); names.sort();
                    for name in names { out.push_str(&format!("  {name}: {}\n", map[name])); }
                } else { out.push_str(&format!("  raw: {flags}\n")); }
            }
            Err(e) => out.push_str(&format!("features: could not read ({e})\n")),
        }
        Ok(bounded(&out, 32_000))
    }
}
export!(Component);
