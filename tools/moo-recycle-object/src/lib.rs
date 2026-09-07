wit_bindgen::generate!({ world: "tool", path: "../../wit", generate_all });

mod moo;

use moo::{bounded, moo_object_expr, Moo};
use serde_json::{json, Value};

struct Component;

fn reject_literal_system_object(value: &str) -> Result<(), String> {
    let value = value.trim();
    if let Some(id) = value.strip_prefix('#') {
        if id.parse::<i64>().ok() == Some(0) {
            return Err("refusing to recycle system object #0".into());
        }
    }
    Ok(())
}

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "moo-recycle-object".into(),
            description: "Permanently recycle a mooR object. System object #0 is unconditionally protected, including when an invalid CURIE resolves to #0.".into(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "object": { "type": "string" },
                    "wizard": { "type": "boolean", "default": false }
                },
                "required": ["object"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["group:moo".into(), "http".into()],
        }
    }

    fn invoke(_: String, args: String, config: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args).map_err(|e| e.to_string())?;
        let raw = args
            .get("object")
            .and_then(Value::as_str)
            .ok_or("missing object")?;

        // Give direct spellings a clear local refusal. The server-side check below
        // remains authoritative because toobj(invalid CURIE) can evaluate to #0.
        reject_literal_system_object(raw)?;
        let object = moo_object_expr(raw)?;
        let wizard = args.get("wizard").and_then(Value::as_bool).unwrap_or(false);

        // Resolve once inside the same task that recycles. Never call recycle() on
        // the expression directly: invalid toobj() values collapse to #0 in mooR.
        // The equality check is deliberately before valid(), so this remains a
        // permanent hard stop even on databases where #0 is currently valid.
        let source = format!(
            "target = {object}; \
             if (target == #0) raise(E_PERM, \"Refusing to recycle system object #0.\"); endif \
             if (!valid(target)) raise(E_INVARG, \"Refusing to recycle an invalid object reference.\"); endif \
             recycle(target); return target;"
        );
        let result = Moo::from_config(&config)?.captured(
            "/v1/eval",
            &source,
            None,
            wizard,
        )?;

        Ok(bounded(
            &json!({
                "success": result.success,
                "error": result.error,
                "output": result.output,
                "value": result.value
            })
            .to_string(),
            32_000,
        ))
    }
}

export!(Component);

#[cfg(test)]
mod tests {
    use super::reject_literal_system_object;

    #[test]
    fn rejects_all_numeric_spellings_of_zero() {
        for value in ["#0", " #0 ", "#00", "#-0"] {
            assert!(reject_literal_system_object(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn permits_nonzero_and_indirect_references_for_server_side_validation() {
        for value in ["#1", "#-1", "moor:system", "uuid:001122-AABB"] {
            assert!(reject_literal_system_object(value).is_ok(), "rejected {value}");
        }
    }
}
