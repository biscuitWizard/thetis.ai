---
name = "The mooR FlatBuffers wire schema"
brief = "How to change a .fbs schema without breaking a running cluster or existing database: the generation commands, and the rules for adding and deprecating fields."
when_to_use = "Use before editing any .fbs schema file, regenerating bindings, adding a field, a union variant or an enum value, or diagnosing a decode error or version mismatch between a daemon and a host. Not for the meaning of a particular RPC message (read daemon-and-rpc), and not for the MOO Var type itself (read moor/language-and-compiler/value-model) or Thetis internals."
universal = false
tags = ["moor", "flatbuffers", "fbs", "schema", "planus", "flatc", "wire format", "compatibility", "schema evolution", "moor-schema", "generated code", "crates/schema", "moor_rpc.fbs", "common.fbs", "var.fbs", "task.fbs", "moor_event_log.fbs", "all_schemas.fbs", "connections.fbs", "schemas_generated.rs", "schema:build", "renumbering"]
version = 2
---
