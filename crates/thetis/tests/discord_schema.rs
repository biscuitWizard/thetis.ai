//! Prints the slash-command registration payload, so the exact JSON sent to
//! Discord can be eyeballed without a bot token.
//!
//!     cargo test -p thetis --test discord_schema -- --nocapture

use thetis::config::Config;

/// The payload with everything switched off, and with everything on.
///
/// `/fork` is only advertised when the operator has enabled it, so both shapes
/// are printed and checked — a registration is only as good as the *actual*
/// config it was built from, and the enabled one is the shape that carries the
/// command with permissions attached to it.
fn payloads() -> Vec<(&'static str, serde_json::Value)> {
    let cfg = Config::load().expect("a loadable config");
    let mut off = cfg.discord.clone();
    off.allow_fork = false;
    let mut on = cfg.discord.clone();
    on.allow_fork = true;
    vec![
        ("default", thetis::discord::commands::schema(&off)),
        ("allow_fork", thetis::discord::commands::schema(&on)),
    ]
}

#[test]
fn the_registration_payload_is_well_formed() {
    for (label, schema) in payloads() {
        println!("--- {label} ---");
        println!("{}", serde_json::to_string_pretty(&schema).unwrap());

        let commands = schema.as_array().expect("an array");
        assert!(!commands.is_empty());
        // Discord allows 100 global CHAT_INPUT commands.
        assert!(commands.len() <= 100);
        let names: Vec<&str> = commands.iter().map(|c| c["name"].as_str().unwrap()).collect();
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "command names must be unique: {names:?}"
        );

        // Every command must name its contexts. Omitting them registers fine and
        // then never appears in the picker, which is the failure this test exists
        // to keep from coming back. See `discord::commands::schema`.
        for command in commands {
            let name = command["name"].as_str().unwrap();
            assert_eq!(
                command["contexts"],
                serde_json::json!([0, 1]),
                "/{name} must name GUILD and BOT_DM explicitly"
            );
            assert_eq!(
                command["integration_types"],
                serde_json::json!([0]),
                "/{name} must be guild-install only"
            );
        }
    }
}

#[test]
fn fork_is_advertised_only_when_it_is_enabled() {
    let payloads = payloads();
    let names = |label: &str| -> Vec<String> {
        payloads
            .iter()
            .find(|(l, _)| *l == label)
            .unwrap()
            .1
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(!names("default").contains(&"fork".to_string()));
    assert!(names("allow_fork").contains(&"fork".to_string()));
}
