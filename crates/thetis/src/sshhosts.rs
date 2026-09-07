//! The named-SSH-host registry.
//!
//! A remote shell needs somewhere to keep *where* it connects: an address, a
//! user, a key, whatever `-o` options a particular box needs. Two properties
//! decide where that somewhere is.
//!
//! * **It must not be publishable by accident.** Host names, internal
//!   addresses and key paths are the shape of an infrastructure map, and
//!   `thetis.toml` is committed and pushed. So the registry lives in
//!   `ssh-hosts.local.toml`, which `*.local.toml` in `.gitignore` already
//!   covers, and which the publish filter therefore never sees.
//! * **It must not be able to break startup.** The config loader reads exactly
//!   `<stem>.local.toml` beside the config file; a *differently* named file is
//!   invisible to it. A malformed registry can therefore fail its own read and
//!   nothing else — whereas a malformed `thetis.local.toml` is unbootable, and
//!   that is the one failure with no in-band cure.
//!
//! It also means the registry never appears in `list_config`, which walks the
//! committed file only. Nothing here is reachable by asking for a setting.
//!
//! The file is shared rather than per-branch: hosts are facts about the world,
//! not about a conversation's checkout, so a host added in one conversation is
//! usable from every other one. `store_path` therefore prefers the directory of
//! the shared overlay a worker is pointed at over its own worktree.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::Config;

/// Where a remote session connects, under a name the agent chooses.
///
/// Everything is optional except `name` and `host`, because ssh's own config
/// already answers most of it for a box that is set up properly. An empty
/// field means "let ssh decide" rather than a default invented here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshHost {
    #[serde(skip)]
    pub name: String,
    /// Hostname or address. Required.
    pub host: String,
    /// 0 means ssh's default, which respects `~/.ssh/config`.
    pub port: u32,
    /// Empty means ssh's default.
    pub user: String,
    /// Path to a private key. Empty means ssh's own key discovery.
    pub identity_file: String,
    /// Extra ssh options, each a complete argument such as
    /// `-oStrictHostKeyChecking=accept-new`.
    pub options: Vec<String>,
    /// Directory to enter on connecting. Empty means the login directory.
    pub remote_cwd: String,
    /// Allocate a remote terminal (`ssh -tt`).
    ///
    /// Off by default, and that default matters: without a pty the remote shell
    /// is a plain pipe reader, so it does not echo commands and prints no
    /// prompt, and the marker protocol reads exactly as it does locally. With
    /// one you get echo and prompts mixed into the output — worth it only when
    /// something on the far side demands a terminal, such as `sudo` asking for
    /// a password, or when you need to interrupt a remote command, which is
    /// only deliverable down a pty.
    pub pty: bool,
    /// Free text for the agent's own benefit.
    pub description: String,
}

impl SshHost {
    /// The ssh destination, `user@host` when a user is set.
    pub fn destination(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// One line, for listings. Never prints a key's contents — only its path,
    /// which is what a human needs to recognise it.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.destination()];
        if self.port != 0 {
            parts.push(format!("port {}", self.port));
        }
        if !self.identity_file.is_empty() {
            parts.push(format!("key {}", self.identity_file));
        }
        if !self.remote_cwd.is_empty() {
            parts.push(format!("cwd {}", self.remote_cwd));
        }
        if self.pty {
            parts.push("pty".into());
        }
        if !self.options.is_empty() {
            parts.push(format!("{} option(s)", self.options.len()));
        }
        let mut line = format!("{}  {}", self.name, parts.join("  ·  "));
        if !self.description.is_empty() {
            line.push_str(&format!("\n    {}", self.description));
        }
        line
    }

    /// The full ssh argument list for this host, before the remote command.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();

        // Never prompt. A password prompt on a pipe with nobody to answer it
        // hangs the session until its timeout and reports nothing useful; a
        // refusal names the problem immediately.
        args.push("-oBatchMode=yes".into());
        // A dead network should surface as an error rather than as a session
        // that is neither alive nor gone.
        args.push("-oServerAliveInterval=15".into());
        args.push("-oServerAliveCountMax=3".into());
        args.push("-oConnectTimeout=15".into());

        if self.port != 0 {
            args.push("-p".into());
            args.push(self.port.to_string());
        }
        if !self.identity_file.is_empty() {
            args.push("-i".into());
            args.push(self.identity_file.clone());
            // With an explicit key, stop ssh offering every other key it finds:
            // some servers close the connection after too many attempts.
            args.push("-oIdentitiesOnly=yes".into());
        }
        // The caller's own options come last so they can override anything
        // above.
        args.extend(self.options.iter().cloned());

        if self.pty {
            // Doubled: ssh only allocates a terminal for a *command* when asked
            // twice, and we always pass a command.
            args.push("-tt".into());
        } else {
            args.push("-T".into());
        }

        args.push(self.destination());
        args
    }

    /// The remote command that becomes the session's shell.
    ///
    /// A `cd` into the host's working directory rides along here rather than
    /// being sent as a first command, so the session is already in the right
    /// place before anything else runs.
    pub fn remote_shell_command(&self) -> String {
        let mut script = String::new();
        if self.pty {
            // Without this the pty echoes every command we write, and each one
            // comes back as output the agent did not ask for.
            script.push_str("stty -echo 2>/dev/null; ");
        }
        if !self.remote_cwd.is_empty() {
            // Refuse to continue in the wrong directory: a build that runs in
            // the login directory instead of the project is worse than an
            // error.
            script.push_str(&format!(
                "cd {} || exit 1; ",
                shell_quote(&self.remote_cwd)
            ));
        }
        // `exec` so the shell we talk to is the process ssh is watching, and
        // `-s` so it reads commands from stdin whether or not there is a pty.
        script.push_str("exec ${SHELL:-/bin/sh} -s");
        script
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(anyhow!("a host needs a name"));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(anyhow!(
                "host name {:?} must be letters, digits, '-', '_' or '.' — it is used as an \
                 identifier, not as a shell word",
                self.name
            ));
        }
        if self.host.trim().is_empty() {
            return Err(anyhow!("host {:?} needs an address", self.name));
        }
        if self.port > 65535 {
            return Err(anyhow!("port {} is not a port", self.port));
        }
        // Everything here becomes an argv entry rather than shell text, so
        // quoting is not the risk. Whitespace in the middle of one argument is
        // still a mistake worth catching, because ssh will read it as one
        // option and reject it obscurely.
        for option in &self.options {
            if !option.starts_with('-') {
                return Err(anyhow!(
                    "ssh option {option:?} must start with '-'; give complete arguments such as \
                     -oStrictHostKeyChecking=accept-new"
                ));
            }
        }
        Ok(())
    }
}

/// Wraps a string so a POSIX shell reads it as one literal word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    hosts: BTreeMap<String, SshHost>,
}

/// Where the registry lives.
///
/// Beside the shared local overlay when a worker has been pointed at one, so
/// every conversation sees the same hosts; otherwise beside the config file.
pub fn store_path(cfg: &Config) -> PathBuf {
    let overlay = match std::env::var("THETIS_LOCAL_CONFIG") {
        Ok(shared) if !shared.trim().is_empty() => PathBuf::from(shared),
        _ => cfg.local_overlay(),
    };
    overlay.with_file_name("ssh-hosts.local.toml")
}

fn read(cfg: &Config) -> Result<StoreFile> {
    let path = store_path(cfg);
    if !path.is_file() {
        return Ok(StoreFile::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut store: StoreFile = toml::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    // The name is the table key, so it is not stored twice.
    for (name, host) in store.hosts.iter_mut() {
        host.name = name.clone();
    }
    Ok(store)
}

fn write(cfg: &Config, store: &StoreFile) -> Result<PathBuf> {
    let path = store_path(cfg);
    let text = format!(
        "# Named SSH hosts for Thetis terminal sessions.\n\
         #\n\
         # Written by the ssh_host_* tools. This file is deliberately NOT \
         thetis.local.toml:\n\
         # it is gitignored by *.local.toml, invisible to the config loader, and \
         so cannot\n\
         # be published by accident or stop Thetis from starting.\n\n{}",
        toml::to_string(store).context("serialising the host registry")?
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    // It names keys and internal addresses; nobody else on the box needs it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

/// Every host, by name.
pub fn list(cfg: &Config) -> Result<Vec<SshHost>> {
    Ok(read(cfg)?.hosts.into_values().collect())
}

/// One host by name, with a message that lists the alternatives when it is
/// missing — a typo in a host name is otherwise a dead end.
pub fn get(cfg: &Config, name: &str) -> Result<SshHost> {
    let store = read(cfg)?;
    match store.hosts.get(name) {
        Some(host) => Ok(host.clone()),
        None if store.hosts.is_empty() => Err(anyhow!(
            "no ssh host named {name:?}, and none are defined yet. Add one with \
             ssh_host_set."
        )),
        None => Err(anyhow!(
            "no ssh host named {name:?}. Defined: {}",
            store
                .hosts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Adds a host, or replaces one field-for-field.
///
/// `merge` keeps whatever the caller left unset, so editing one field of an
/// existing host does not require restating the rest.
pub fn set(cfg: &Config, host: SshHost, merge: bool) -> Result<String> {
    let mut store = read(cfg)?;
    let name = host.name.trim().to_string();
    let existing = store.hosts.get(&name).cloned();

    let mut final_host = match (&existing, merge) {
        (Some(old), true) => {
            let mut merged = old.clone();
            if !host.host.is_empty() {
                merged.host = host.host;
            }
            if host.port != 0 {
                merged.port = host.port;
            }
            if !host.user.is_empty() {
                merged.user = host.user;
            }
            if !host.identity_file.is_empty() {
                merged.identity_file = host.identity_file;
            }
            if !host.options.is_empty() {
                merged.options = host.options;
            }
            if !host.remote_cwd.is_empty() {
                merged.remote_cwd = host.remote_cwd;
            }
            if !host.description.is_empty() {
                merged.description = host.description;
            }
            // A bool cannot say "unset", so it is taken as given.
            merged.pty = host.pty;
            merged
        }
        _ => host,
    };
    final_host.name = name.clone();
    final_host.validate()?;

    let verb = if existing.is_some() { "updated" } else { "added" };
    store.hosts.insert(name.clone(), final_host.clone());
    let path = write(cfg, &store)?;

    tracing::info!(host = %name, "ssh host {verb}");
    Ok(format!(
        "{verb} ssh host {name}\n  {}\nstored in {} (gitignored, never published)",
        final_host.summary(),
        path.display()
    ))
}

pub fn remove(cfg: &Config, name: &str) -> Result<String> {
    let mut store = read(cfg)?;
    if store.hosts.remove(name).is_none() {
        return Err(anyhow!("no ssh host named {name:?}"));
    }
    write(cfg, &store)?;
    tracing::info!(host = %name, "ssh host removed");
    Ok(format!("removed ssh host {name}"))
}

/// Renames a host, keeping its settings.
pub fn rename(cfg: &Config, from: &str, to: &str) -> Result<String> {
    let mut store = read(cfg)?;
    let mut host = store
        .hosts
        .remove(from)
        .ok_or_else(|| anyhow!("no ssh host named {from:?}"))?;
    let to = to.trim().to_string();
    if store.hosts.contains_key(&to) {
        return Err(anyhow!("there is already a host named {to:?}"));
    }
    host.name = to.clone();
    host.validate()?;
    store.hosts.insert(to.clone(), host);
    write(cfg, &store)?;
    Ok(format!("renamed ssh host {from} to {to}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> SshHost {
        SshHost {
            name: "prod".into(),
            host: "10.0.0.5".into(),
            user: "deploy".into(),
            port: 2222,
            identity_file: "/keys/prod".into(),
            remote_cwd: "/srv/app".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_argument_list_is_batch_mode_and_names_the_destination_last() {
        let args = host().ssh_args();
        assert!(args.contains(&"-oBatchMode=yes".to_string()));
        assert!(args.contains(&"-oIdentitiesOnly=yes".to_string()));
        assert_eq!(args.last().unwrap(), "deploy@10.0.0.5");
        // No pty unless asked: a pty echoes commands and prints prompts, and
        // the marker protocol reads cleanly without one.
        assert!(args.contains(&"-T".to_string()));
        assert!(!args.contains(&"-tt".to_string()));
    }

    #[test]
    fn a_pty_host_asks_twice_and_silences_the_echo() {
        let mut h = host();
        h.pty = true;
        assert!(h.ssh_args().contains(&"-tt".to_string()));
        assert!(h.remote_shell_command().starts_with("stty -echo"));
    }

    #[test]
    fn the_remote_working_directory_is_quoted_and_fatal_if_missing() {
        let mut h = host();
        h.remote_cwd = "/srv/it's here".into();
        let script = h.remote_shell_command();
        assert!(script.contains(r"'/srv/it'\''s here'"), "{script}");
        // Landing in the wrong directory silently is worse than failing.
        assert!(script.contains("|| exit 1"), "{script}");
        assert!(script.ends_with("-s"), "{script}");
    }

    #[test]
    fn a_host_without_a_user_leaves_the_choice_to_ssh() {
        let mut h = host();
        h.user = String::new();
        assert_eq!(h.destination(), "10.0.0.5");
    }

    #[test]
    fn a_name_that_is_not_an_identifier_is_refused() {
        let mut h = host();
        h.name = "prod; rm -rf /".into();
        let err = h.validate().unwrap_err().to_string();
        assert!(err.contains("must be letters"), "{err}");
    }

    #[test]
    fn an_option_that_is_not_an_option_is_refused() {
        let mut h = host();
        h.options = vec!["StrictHostKeyChecking=no".into()];
        let err = h.validate().unwrap_err().to_string();
        assert!(err.contains("must start with '-'"), "{err}");
    }

    #[test]
    fn the_store_never_sits_where_config_or_git_would_pick_it_up() {
        let cfg = Config::load().unwrap();
        let path = store_path(&cfg);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Gitignored by `*.local.toml`, and not the `<stem>.local.toml` the
        // config loader reads — so it can neither be published nor stop a
        // start.
        assert!(name.ends_with(".local.toml"), "{name}");
        assert_ne!(path, cfg.local_overlay());
    }

    #[test]
    fn round_trip_through_the_file_keeps_every_field_and_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        // Point the store at a scratch directory by moving the config path.
        cfg.config_path = dir.path().join("thetis.toml");
        std::env::remove_var("THETIS_LOCAL_CONFIG");

        set(&cfg, host(), false).unwrap();
        let listed = list(&cfg).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], host(), "every field must survive the round trip");

        // A merge edit leaves the rest alone.
        let patch = SshHost {
            name: "prod".into(),
            remote_cwd: "/srv/other".into(),
            ..Default::default()
        };
        set(&cfg, patch, true).unwrap();
        let after = get(&cfg, "prod").unwrap();
        assert_eq!(after.remote_cwd, "/srv/other");
        assert_eq!(after.user, "deploy", "the merge must not clear other fields");
        assert_eq!(after.port, 2222);

        rename(&cfg, "prod", "prod-eu").unwrap();
        assert!(get(&cfg, "prod").is_err());
        assert_eq!(get(&cfg, "prod-eu").unwrap().user, "deploy");

        remove(&cfg, "prod-eu").unwrap();
        assert!(list(&cfg).unwrap().is_empty());
    }

    #[test]
    fn a_missing_host_names_the_ones_that_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.config_path = dir.path().join("thetis.toml");
        std::env::remove_var("THETIS_LOCAL_CONFIG");

        set(&cfg, host(), false).unwrap();
        let err = get(&cfg, "prd").unwrap_err().to_string();
        assert!(err.contains("Defined: prod"), "{err}");
    }
}
