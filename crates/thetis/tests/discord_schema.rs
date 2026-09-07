//! Prints the slash-command registration payload, so the exact JSON sent to
//! Discord can be eyeballed without a bot token.
//!
//!     cargo test -p thetis --test discord_schema -- --nocapture

#[test]
fn the_registration_payload_is_well_formed() {
    let schema = thetis::discord::commands::schema();
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());

    let commands = schema.as_array().expect("an array");
    assert!(!commands.is_empty());
    // Discord allows 100 global CHAT_INPUT commands.
    assert!(commands.len() <= 100);
    let names: Vec<&str> = commands.iter().map(|c| c["name"].as_str().unwrap()).collect();
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "command names must be unique: {names:?}");
}
