//! The control panel's frames: one inbound type, `admin`, with an `op`.
//!
//! Every op maps onto one import of the host's `admin` interface and answers
//! with a typed frame — `admin-overview`, `admin-fields`, `admin-entries`,
//! `admin-waits` — or with `admin-result`, which carries an outcome and the
//! op it belongs to, so the panel can show a refusal beside the control that
//! caused it rather than in a toast.
//!
//! Nothing here decides who may do what. The host refuses a caller who is
//! not an administrator on every import, and `available` lets the panel know
//! that up front. Adding an op is one arm in `dispatch`.
//!
//! Every write is applied by the host as it lands; the result message says
//! what took effect and what waits for a restart, and the `admin-overview`
//! that follows each write carries the pending list for the banner.

use crate::handlers::reply;
use crate::thetis::grip::admin as host;
use crate::GatewayAction;
use serde_json::{json, Value};

fn field(frame: &Value, key: &str) -> String {
    frame
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The outcome of a mutating op, named so the panel can place it.
fn result(op: &str, outcome: Result<String, String>, extra: Value) -> GatewayAction {
    let mut body = json!({
        "type": "admin-result",
        "op": op,
        "ok": outcome.is_ok(),
        "message": match &outcome { Ok(m) => m, Err(e) => e },
    });
    if let (Some(obj), Some(more)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            obj.insert(k.clone(), v.clone());
        }
    }
    reply(body)
}

fn refused(op: &str, why: String) -> Vec<GatewayAction> {
    vec![result(op, Err(why), json!({}))]
}

fn overview() -> GatewayAction {
    match host::overview() {
        Ok(view) => {
            let mut body = serde_json::to_value(&view).unwrap_or_else(|_| json!({}));
            if let Some(obj) = body.as_object_mut() {
                obj.insert("type".into(), json!("admin-overview"));
                obj.insert(
                    "actions".into(),
                    serde_json::to_value(host::actions()).unwrap_or_else(|_| json!([])),
                );
            }
            reply(body)
        }
        Err(why) => result("overview", Err(why), json!({})),
    }
}

fn waits() -> GatewayAction {
    match host::waits() {
        Ok(raw) => {
            let mut body: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
            if let Some(obj) = body.as_object_mut() {
                obj.insert("type".into(), json!("admin-waits"));
            }
            reply(body)
        }
        Err(why) => result("waits", Err(why), json!({})),
    }
}

/// The settings, with everything the form needs: the schema's sections in
/// order, then every field. A prefix narrows the fields to one section.
fn fields(prefix: Option<&str>) -> GatewayAction {
    match host::fields(prefix) {
        Ok(all) => reply(json!({
            "type": "admin-fields",
            "prefix": prefix.unwrap_or_default(),
            "sections": host::sections(),
            "fields": all,
        })),
        Err(why) => result("fields", Err(why), json!({})),
    }
}

fn entries(section: &str) -> GatewayAction {
    match host::entries(section) {
        Ok(rows) => reply(json!({
            "type": "admin-entries",
            "section": section,
            "tables": host::tables(),
            "entries": rows.iter().map(|e| json!({
                "id": e.id,
                "source": e.source,
                "fields": serde_json::from_str::<Value>(&e.fields_json).unwrap_or_else(|_| json!({})),
            })).collect::<Vec<_>>(),
        })),
        Err(why) => result("entries", Err(why), json!({})),
    }
}

/// Routes one `admin` frame by its `op`.
pub fn dispatch(frame: &Value) -> Vec<GatewayAction> {
    let op = field(frame, "op");
    if !host::available() {
        return refused(&op, "administrators only".into());
    }
    match op.as_str() {
        "overview" => vec![overview()],
        "waits" => vec![waits()],
        "act" => {
            let action = field(frame, "action");
            let target = field(frame, "target");
            if action.is_empty() {
                return refused("act", "act requires an action".into());
            }
            vec![
                result("act", host::act(&action, &target), json!({ "action": action, "target": target })),
                overview(),
            ]
        }
        "sign-out" => {
            let account = field(frame, "account");
            if account.is_empty() {
                return refused("sign-out", "sign-out requires an account".into());
            }
            let outcome = host::sign_out_everywhere(&account)
                .map(|n| format!("signed {account} out of {n} login(s)"));
            vec![result("sign-out", outcome, json!({ "account": account })), overview()]
        }
        "fields" => {
            let prefix = field(frame, "prefix");
            vec![fields((!prefix.is_empty()).then_some(prefix.as_str()))]
        }
        "set-field" => {
            let key = field(frame, "key");
            if key.is_empty() {
                return refused("set-field", "set-field requires a key".into());
            }
            // The value is taken as sent, untrimmed: a prompt may end in a
            // newline on purpose.
            let value = frame.get("value").and_then(Value::as_str).unwrap_or_default();
            let outcome = host::set_field(&key, value);
            let section = key.split('.').next().unwrap_or_default().to_string();
            // The overview follows because it carries what now awaits a
            // restart, which a write can add to or clear.
            vec![
                result("set-field", outcome, json!({ "key": key })),
                fields(Some(&section)),
                overview(),
            ]
        }
        "entries" => {
            let section = field(frame, "section");
            if section.is_empty() {
                return refused("entries", "entries requires a section".into());
            }
            vec![entries(&section)]
        }
        "save-entry" => {
            let section = field(frame, "section");
            let id = field(frame, "id");
            if section.is_empty() || id.is_empty() {
                return refused("save-entry", "save-entry requires a section and an id".into());
            }
            let fields_json = frame
                .get("fields")
                .cloned()
                .unwrap_or_else(|| json!({}))
                .to_string();
            vec![
                result(
                    "save-entry",
                    host::save_entry(&section, &id, &fields_json),
                    json!({ "section": section, "id": id }),
                ),
                entries(&section),
                overview(),
            ]
        }
        "remove-entry" => {
            let section = field(frame, "section");
            let id = field(frame, "id");
            if section.is_empty() || id.is_empty() {
                return refused("remove-entry", "remove-entry requires a section and an id".into());
            }
            vec![
                result(
                    "remove-entry",
                    host::remove_entry(&section, &id),
                    json!({ "section": section, "id": id }),
                ),
                entries(&section),
                overview(),
            ]
        }
        "reload" => vec![result("reload", host::reload(), json!({})), overview(), fields(None)],
        "restart" => {
            let reason = field(frame, "reason");
            vec![result("restart", host::restart(&reason), json!({}))]
        }
        other => refused(other, format!("unknown admin op: {other}")),
    }
}

#[cfg(test)]
mod tests {
    //! Only what is pure runs here; the ops themselves talk to the host.

    use super::*;

    #[test]
    fn a_result_names_its_op_and_carries_the_extras() {
        let GatewayAction::Reply(text) = result("act", Ok("done".into()), json!({ "action": "x" })) else {
            panic!("a reply");
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "admin-result");
        assert_eq!(v["op"], "act");
        assert_eq!(v["ok"], true);
        assert_eq!(v["message"], "done");
        assert_eq!(v["action"], "x");

        let GatewayAction::Reply(text) = result("restart", Err("no".into()), json!({})) else {
            panic!("a reply");
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((v["ok"].as_bool(), v["message"].as_str()), (Some(false), Some("no")));
    }

    #[test]
    fn frame_fields_are_trimmed_and_default_empty() {
        let frame = json!({ "op": "  act ", "action": "trunk-reset" });
        assert_eq!(field(&frame, "op"), "act");
        assert_eq!(field(&frame, "target"), "");
    }
}
