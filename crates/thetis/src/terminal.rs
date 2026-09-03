//! Long-lived shell sessions on the host.
//!
//! A session keeps its working directory, environment and shell state between
//! commands, which is what separates it from a one-shot exec.
//!
//! Knowing when a command has finished is the hard part of driving a shell over
//! pipes: the stream never ends, so there is nothing to wait for. Each command
//! is therefore followed by an echo of a unique marker, and `run` reads until
//! that marker appears. The marker is what turns an endless stream back into
//! request and response.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::bindings::types::{TerminalInfo, TerminalOutput};
use crate::config::Config;

/// How much of a session's transcript is kept for the UI's terminal drawer.
/// The tail only: a browser joining late wants the last screenful, not the
/// whole history of a `cargo build`.
const MAX_DISPLAY_BYTES: usize = 96 * 1024;

/// The shared prefix of every completion marker, so a watcher can recognise
/// one without knowing which command it belongs to.
const MARKER_PREFIX: &str = "__thetis_done_";

/// One piece of shell activity, as the terminal drawer sees it.
///
/// Deliberately separate from `read`, which *consumes* the buffer because the
/// agent's own `terminal_read` is defined that way: a viewer that stole the
/// agent's output would break the tool it is meant to be showing. So the pump
/// tees every line — once into the agent's buffer, once here.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TerminalFeed {
    pub id: String,
    /// `opened`, `command`, `output`, `exit` or `closed`.
    pub kind: String,
    pub text: String,
    pub cwd: String,
    pub shell: String,
    /// The ssh host, empty for a local shell. Carried on every event so a tab
    /// created by an `opened` frame is labelled without a second lookup.
    pub remote: String,
}

/// A session as the drawer draws it: what `list` reports, plus the transcript
/// so a tab that opens mid-command has something to show.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TerminalView {
    pub id: String,
    pub cwd: String,
    pub shell: String,
    pub alive: bool,
    pub commands: u32,
    pub transcript: String,
    /// The ssh host the session runs on, empty for a local shell. A drawer tab
    /// has to say which machine a command ran on.
    pub remote: String,
    /// The agent's label for the session, empty when it did not give one.
    pub name: String,
    /// The shell's own process id, which is also its process group leader.
    /// Shown in the details card, and the one number that lets someone
    /// correlate a drawer tab with what `ps` on the box is telling them.
    pub pid: i32,
    /// Whether the far side allocated a terminal. Governs whether an interrupt
    /// can be delivered at all, so it belongs in the details.
    pub pty: bool,
    /// The user the shell runs as, for the prompt line.
    pub user: String,
}

/// What to open, gathered into one struct because the argument list had grown
/// past the point where positional `Option`s were readable.
#[derive(Debug, Default, Clone)]
pub struct OpenSpec {
    /// Local working directory, confined to the roots. Ignored for a remote
    /// session, which starts wherever its host says.
    pub cwd: Option<String>,
    /// A label the agent chooses, so `sessions` reads as something other than
    /// `term-1 … term-4`.
    pub name: Option<String>,
    /// Environment variables for the session's shell.
    pub env: Vec<(String, String)>,
    /// The name of a registered ssh host. `None` opens a local shell.
    pub host: Option<String>,
}

impl OpenSpec {
    /// A plain local session in the default directory.
    pub fn local() -> Self {
        Self::default()
    }
}

struct Session {
    id: String,
    /// The agent's label, empty when it did not give one.
    name: String,
    cwd: String,
    shell: String,
    /// The ssh host this session runs on, empty for a local shell.
    ///
    /// Everything downstream keys off this: the working-directory guard is
    /// local-only, a signal has to travel as a control byte rather than a
    /// `kill`, and every listing has to say which machine a command ran on.
    remote: String,
    /// Whether the far side has a terminal. Only a pty can carry an interrupt.
    pty: bool,
    /// Who the shell runs as, for the drawer's `user@host:cwd $` prompt line.
    /// Resolved once at open: for a local shell from the environment, for a
    /// remote one from the ssh host's login user.
    user: String,
    /// The marker a backgrounded command will print when it finishes.
    ///
    /// This is what makes "start it and come back later" work without a second
    /// mechanism: the command is written exactly as a foreground one is, and
    /// the next `read` looks for its marker instead of `run` waiting for it.
    pending: Option<Pending>,
    child: Child,
    /// Behind its own lock so a write can outlive the `sessions` guard: a
    /// shell busy running a command is not reading stdin, and a large script
    /// would otherwise block every *other* terminal call behind the map lock.
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    /// The shell leads its own process group, so everything it starts can be
    /// signalled — and killed — as one. Without this a background process the
    /// agent left running outlives the shell, the worker and the conversation.
    pgid: i32,
    /// Everything the shell has written that `read` has not yet returned.
    buffer: Arc<Mutex<String>>,
    /// The same output, kept for anyone watching rather than consuming: the
    /// browser's terminal drawer. Capped to its tail.
    display: Arc<Mutex<String>>,
    commands: u32,
    last_used: Instant,
}

/// A command left running in the background, and how to recognise its end.
#[derive(Debug, Clone)]
struct Pending {
    marker: String,
    command: String,
    started: Instant,
}

impl Session {
    /// Kills the shell *and* everything it started.
    async fn terminate(&mut self) {
        signal_group(self.pgid, libc::SIGKILL);
        let _ = self.child.kill().await;
    }

}

/// Signals a whole process group, ignoring "nothing there" — the shell may
/// already have gone, and its children with it.
fn signal_group(pgid: i32, sig: i32) {
    if pgid > 1 {
        unsafe {
            libc::killpg(pgid, sig);
        }
    }
}

/// Signals everything in the shell's process group *except the shell*.
///
/// Signalling the group as a whole is wrong for a command timeout: a
/// non-interactive shell does not ignore SIGINT, so it dies along with the
/// runaway and the session is lost — which is exactly what the caller was
/// trying to avoid. Signalling only the descendants ends the stuck command and
/// leaves the shell to read the next one.
fn signal_children(pgid: i32, leader: i32, sig: i32) {
    if pgid <= 1 {
        return;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid == leader {
            continue;
        }
        // `/proc/<pid>/stat` is `pid (comm) state ppid pgrp ...`, and `comm`
        // can contain spaces and parentheses — so split after the *last* ')'.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(tail) = stat.rfind(')').map(|at| &stat[at + 1..]) else {
            continue;
        };
        let fields: Vec<&str> = tail.split_whitespace().collect();
        if fields.get(2).and_then(|p| p.parse::<i32>().ok()) == Some(pgid) {
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }
}

pub struct Terminals {
    sessions: tokio::sync::Mutex<HashMap<String, Session>>,
    counter: std::sync::atomic::AtomicU64,
    /// Shell activity, for anything that wants to watch rather than drive: the
    /// worker mirrors this up to the gateway, which fans it out to the browser
    /// tabs on that conversation. A broadcast channel because there may be no
    /// subscriber at all — the common case — and dropping into a void must cost
    /// nothing and block nobody.
    feed_tx: tokio::sync::broadcast::Sender<TerminalFeed>,
}

impl Default for Terminals {
    fn default() -> Self {
        let (feed_tx, _) = tokio::sync::broadcast::channel(512);
        Self {
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(0),
            feed_tx,
        }
    }
}

impl Terminals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribes to shell activity across every session in this worker.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TerminalFeed> {
        self.feed_tx.subscribe()
    }

    fn announce(&self, id: &str, kind: &str, text: String, cwd: &str, shell: &str, remote: &str) {
        let _ = self.feed_tx.send(TerminalFeed {
            id: id.to_string(),
            kind: kind.to_string(),
            text,
            cwd: cwd.to_string(),
            shell: shell.to_string(),
            remote: remote.to_string(),
        });
    }

    /// Every session with its transcript, for a browser tab that has just
    /// opened the drawer and has no history of its own.
    pub async fn views(&self) -> Vec<TerminalView> {
        let mut sessions = self.sessions.lock().await;
        let mut out: Vec<TerminalView> = sessions
            .values_mut()
            .map(|s| TerminalView {
                id: s.id.clone(),
                cwd: s.cwd.clone(),
                shell: s.shell.clone(),
                alive: matches!(s.child.try_wait(), Ok(None)),
                commands: s.commands,
                remote: s.remote.clone(),
                name: s.name.clone(),
                pid: s.pgid,
                pty: s.pty,
                user: s.user.clone(),
                transcript: s
                    .display
                    .lock()
                    .map(|d| d.clone())
                    .unwrap_or_default(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn require_enabled(cfg: &Config) -> Result<()> {
        if cfg.terminal.enabled {
            Ok(())
        } else {
            Err(anyhow!(
                "terminal access is off; set terminal.enabled in thetis.toml to turn it on"
            ))
        }
    }

    // --- lifecycle ---------------------------------------------------------

    pub async fn open(&self, cfg: &Config, spec: OpenSpec) -> Result<String> {
        Self::require_enabled(cfg)?;

        let mut sessions = self.sessions.lock().await;
        self.reap(&mut sessions, cfg);

        if sessions.len() >= cfg.terminal.max_sessions {
            return Err(anyhow!(
                "already at the limit of {} terminal sessions; close one first",
                cfg.terminal.max_sessions
            ));
        }

        // A remote host is looked up before anything is spawned, so a typo in a
        // host name costs nothing.
        let host = match spec.host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(name) => {
                if !cfg.terminal.ssh_enabled {
                    return Err(anyhow!(
                        "remote shells are off; set terminal.ssh_enabled in thetis.toml to \
                         turn them on"
                    ));
                }
                Some(crate::sshhosts::get(cfg, name)?)
            }
            None => None,
        };

        // The local working directory goes through the same confinement as the
        // filesystem tools, so a session cannot start outside the roots. It is
        // also where ssh itself is launched from, which is harmless and keeps
        // relative `-i` paths meaning what they look like.
        let dir = match spec.cwd.as_deref() {
            Some(raw) => crate::hostfs::resolve(cfg, raw)?,
            None => cfg
                .filesystem
                .roots
                .first()
                .cloned()
                .unwrap_or_else(|| cfg.root.clone()),
        };
        if !dir.is_dir() {
            return Err(anyhow!("{} is not a directory", dir.display()));
        }

        let mut command = match &host {
            Some(host) => {
                let mut command = Command::new(&cfg.terminal.ssh_program);
                command.args(host.ssh_args());
                command.arg(host.remote_shell_command());
                command
            }
            None => {
                let mut command = Command::new(&cfg.terminal.shell);
                command.args(&cfg.terminal.shell_args);
                command
            }
        };
        command
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Rejected up front rather than passed to the shell: a name with '=' or
        // a NUL in it is a mistake, and an unset variable the caller believes
        // is set is a worse outcome than an error.
        for (key, value) in &spec.env {
            if key.trim().is_empty() || key.contains('=') || key.contains('\0') {
                return Err(anyhow!("{key:?} is not a usable environment variable name"));
            }
            command.env(key, value);
        }
        // After fork, before exec. The child is not a group leader yet, so
        // `setsid` succeeds and makes it one; every descendant then shares its
        // group id and dies with it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let launched = match &host {
            Some(host) => format!("{} to {}", cfg.terminal.ssh_program, host.destination()),
            None => cfg.terminal.shell.clone(),
        };
        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("cannot start {launched}: {e}"))?;

        let pgid = child.id().unwrap_or(0) as i32;
        let stdin = Arc::new(tokio::sync::Mutex::new(
            child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?,
        ));
        let buffer = Arc::new(Mutex::new(String::new()));
        let display = Arc::new(Mutex::new(String::new()));

        let id = format!(
            "term-{}",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1
        );

        // Both streams feed one buffer, so interleaved output reads the way it
        // would in a real terminal. The tee into `display` and the feed is what
        // the drawer renders; the agent's own buffer is untouched.
        let watch = Watch {
            display: display.clone(),
            feed: self.feed_tx.clone(),
            id: id.clone(),
        };
        if let Some(stdout) = child.stdout.take() {
            pump(
                stdout,
                buffer.clone(),
                cfg.terminal.max_output_bytes,
                watch.clone(),
            );
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, buffer.clone(), cfg.terminal.max_output_bytes, watch);
        }

        // Hoisted out of the struct literal because the drawer's `opened`
        // event needs the same three values, and a remote session's are not
        // simply `dir` and the configured shell.
        //
        // A remote session's directory is unknown until the far side says so;
        // the first command's marker fills it in.
        let cwd = match &host {
            Some(host) if !host.remote_cwd.is_empty() => host.remote_cwd.clone(),
            Some(_) => "(remote)".to_string(),
            None => dir.display().to_string(),
        };
        let shell = match &host {
            Some(host) => format!("ssh {}", host.destination()),
            None => cfg.terminal.shell.clone(),
        };
        let remote = host.as_ref().map(|h| h.name.clone()).unwrap_or_default();
        // A remote host may not state a user, in which case ssh falls back to
        // the local one — same rule as ssh's, so the prompt cannot claim a
        // login that is not what happened.
        let user = match &host {
            Some(host) if !host.user.is_empty() => host.user.clone(),
            _ => std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_default(),
        };

        sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                name: spec.name.unwrap_or_default().trim().to_string(),
                cwd: cwd.clone(),
                shell: shell.clone(),
                remote: remote.clone(),
                pty: host.as_ref().is_some_and(|h| h.pty),
                user,
                pending: None,
                child,
                stdin,
                buffer,
                display,
                pgid,
                commands: 0,
                last_used: Instant::now(),
            },
        );
        drop(sessions);

        tracing::info!(
            terminal = %id,
            dir = %dir.display(),
            remote = %remote,
            "terminal session opened"
        );

        // Announced before the connection check, not after, so that a remote
        // session which fails to come up still shows its tab — the ssh error
        // arrives as ordinary output, and a tab that appears and then says
        // "exited" explains the failure far better than one that never appears.
        self.announce(&id, "opened", String::new(), &cwd, &shell, &remote);

        // An ssh session that cannot connect otherwise looks like a healthy
        // session whose every command times out — and the reason ssh gives (a
        // refused key, an unknown host) is on stderr at connect time and gone
        // by the time anyone asks. So prove the far side answers before handing
        // the id back, and report ssh's own words if it does not.
        if host.is_some() {
            if let Err(e) = self.confirm_remote(cfg, &id).await {
                let _ = self.close(&id).await;
                return Err(e);
            }
        }

        Ok(id)
    }

    /// Runs one trivial command to prove a remote session is usable.
    ///
    /// On failure the message carries whatever ssh printed, because that text
    /// is the only thing that distinguishes "host is down" from "your key was
    /// refused" from "host key changed".
    async fn confirm_remote(&self, cfg: &Config, id: &str) -> Result<()> {
        let probe = self
            .run(cfg, id, "printf ''", cfg.terminal.ssh_connect_timeout)
            .await?;
        if !probe.timed_out {
            return Ok(());
        }
        let noise = probe.output.trim();
        Err(anyhow!(
            "the ssh session did not become usable within {}s{}",
            cfg.terminal.ssh_connect_timeout.as_secs(),
            if noise.is_empty() {
                ". ssh printed nothing — check the host is reachable and that the key is \
                 accepted (BatchMode is on, so ssh will not prompt)."
                    .to_string()
            } else {
                format!(". ssh said:\n{noise}")
            }
        ))
    }

    pub async fn close(&self, id: &str) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let Some(mut session) = sessions.remove(id) else {
            return Err(anyhow!("no terminal session {id}"));
        };
        session.terminate().await;
        let (cwd, shell, remote) = (
            session.cwd.clone(),
            session.shell.clone(),
            session.remote.clone(),
        );
        drop(sessions);
        tracing::info!(terminal = %id, "terminal session closed");
        self.announce(id, "closed", String::new(), &cwd, &shell, &remote);
        Ok(format!("closed {id}"))
    }

    pub async fn list(&self) -> Vec<TerminalInfo> {
        let mut sessions = self.sessions.lock().await;
        let mut out: Vec<TerminalInfo> = sessions
            .values_mut()
            .map(|s| TerminalInfo {
                id: s.id.clone(),
                name: s.name.clone(),
                cwd: s.cwd.clone(),
                shell: s.shell.clone(),
                remote: s.remote.clone(),
                // `try_wait` reports without blocking; `Some` means it exited.
                alive: matches!(s.child.try_wait(), Ok(None)),
                commands: s.commands,
                busy: s.pending.as_ref().map(|p| p.command.clone()).unwrap_or_default(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Returns and clears whatever the shell has written since the last read.
    ///
    /// This is also where a backgrounded command is noticed to have finished:
    /// its marker is sitting in the buffer, and consuming it here is what stops
    /// it from appearing as stray output later, and what lets the session go
    /// back to accepting foreground commands.
    pub async fn read(&self, id: &str) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("no terminal session {id}"))?;
        session.last_used = Instant::now();

        let Some(pending) = session.pending.clone() else {
            return Ok(take(&session.buffer));
        };

        match split_at_marker(&session.buffer, &pending.marker) {
            Some((output, completion)) => {
                let note = completion.note(&session.cwd);
                if let Some(cwd) = completion.cwd {
                    session.cwd = cwd;
                }
                session.pending = None;
                Ok(format!(
                    "{output}{note}\n\n[the background command finished after {}s: {}]",
                    pending.started.elapsed().as_secs(),
                    pending.command
                ))
            }
            None => {
                let so_far = take(&session.buffer);
                Ok(format!(
                    "{so_far}\n\n[still running after {}s: {}]",
                    pending.started.elapsed().as_secs(),
                    pending.command
                ))
            }
        }
    }

    // --- signals -----------------------------------------------------------

    /// Sends a signal to whatever the session is running, sparing the shell.
    ///
    /// This is the deliberate Ctrl-C the surface was missing: the timeout path
    /// kills a runaway, but there was no way to interrupt a command on purpose
    /// — to stop a `tail -f`, or to end a test run whose first failure already
    /// told you what you needed.
    ///
    /// Local sessions get a real signal to the process group. A remote session
    /// cannot: the processes are on another machine and there is nothing here
    /// to `kill`. Down a pty the control byte is the signal, so `SIGINT` and
    /// `SIGTSTP` travel as `0x03` and `0x1a`; without a pty there is no channel
    /// at all, which is worth saying plainly rather than pretending to deliver.
    pub async fn signal(&self, cfg: &Config, id: &str, signal: &str) -> Result<String> {
        Self::require_enabled(cfg)?;

        let name = signal.trim().to_ascii_uppercase();
        let name = name.strip_prefix("SIG").unwrap_or(&name);

        let (stdin, pgid, leader, remote, pty) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| anyhow!("no terminal session {id}"))?;
            session.last_used = Instant::now();
            (
                session.stdin.clone(),
                session.pgid,
                session.child.id().unwrap_or(0) as i32,
                session.remote.clone(),
                session.pty,
            )
        };

        if remote.is_empty() {
            let sig = match name {
                "INT" => libc::SIGINT,
                "TERM" => libc::SIGTERM,
                "TSTP" => libc::SIGTSTP,
                "HUP" => libc::SIGHUP,
                "QUIT" => libc::SIGQUIT,
                "KILL" => libc::SIGKILL,
                other => {
                    return Err(anyhow!(
                        "unknown signal {other:?}; use INT, TERM, TSTP, HUP, QUIT or KILL"
                    ))
                }
            };
            // Children only, never the shell. Signalling the group as a whole
            // takes the shell with it and the session is lost — which is the
            // opposite of what interrupting one command is for.
            signal_children(pgid, leader, sig);
        } else {
            let byte: u8 = match name {
                "INT" => 0x03,
                "QUIT" => 0x1c,
                "TSTP" => 0x1a,
                other => {
                    return Err(anyhow!(
                        "on the remote session {id} ({remote}) only INT, QUIT and TSTP can be \
                         delivered, as control characters — {other:?} would have to be sent by \
                         something running on that host. Close the session to end everything."
                    ))
                }
            };
            if !pty {
                return Err(anyhow!(
                    "session {id} runs on {remote} without a terminal, so there is no channel \
                     for a signal. Set pty=true on that host (ssh_host action=set) to make \
                     interrupts possible, or run `pkill` on the far side."
                ));
            }
            let mut stdin = stdin.lock().await;
            stdin
                .write_all(&[byte])
                .await
                .map_err(|e| anyhow!("cannot signal {id}: {e}"))?;
            stdin.flush().await.ok();
        }

        // A signalled command usually prints something — "^C", a stack trace, a
        // summary — and that is the most useful thing to hand back.
        //
        // Through `read`, not straight out of the buffer: an interrupted
        // background command is *finished*, and its marker is in that buffer.
        // Draining it blindly would swallow the marker and leave the session
        // marked busy for ever, refusing every later command.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let printed = self.read(id).await.unwrap_or_default();
        tracing::info!(terminal = %id, signal = %name, "signalled a session");
        Ok(if printed.trim().is_empty() {
            format!("sent SIG{name} to {id}")
        } else {
            format!("sent SIG{name} to {id}\n\n{printed}")
        })
    }

    /// Writes raw input to a session without waiting for a marker.
    ///
    /// The marker protocol is what makes `run` reliable, and it is also what
    /// makes `run` useless for anything interactive: a program sitting at a
    /// prompt never returns, so the marker never arrives, so the command
    /// "times out" while the program waits for the answer nobody can send.
    /// This is the escape hatch — a password, a `y`, a line of Python, a bare
    /// newline — and it deliberately returns immediately. Collect what came
    /// back with `read`.
    pub async fn send(
        &self,
        cfg: &Config,
        id: &str,
        text: &str,
        submit: bool,
        settle: Duration,
    ) -> Result<String> {
        Self::require_enabled(cfg)?;

        let stdin = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| anyhow!("no terminal session {id}"))?;
            if matches!(session.child.try_wait(), Ok(Some(_))) {
                return Err(anyhow!("terminal session {id} has exited; open a new one"));
            }
            session.last_used = Instant::now();
            session.stdin.clone()
        };

        let payload = if submit {
            format!("{text}\n")
        } else {
            text.to_string()
        };
        {
            let mut stdin = stdin.lock().await;
            let write = async {
                stdin.write_all(payload.as_bytes()).await?;
                stdin.flush().await
            };
            match tokio::time::timeout(Duration::from_secs(10), write).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(anyhow!("cannot write to {id}: {e}")),
                Err(_) => {
                    return Err(anyhow!(
                        "session {id} would not accept the input — it is not reading stdin"
                    ))
                }
            }
        }

        // Long enough for a prompt to come back, short enough not to feel like
        // a wait. Whatever has not arrived yet is picked up by the next read.
        //
        // Through `read` for the same reason as `signal`: the input just sent
        // is often the very thing that lets a backgrounded command finish, and
        // its marker must be recognised rather than drained away.
        tokio::time::sleep(settle).await;
        let printed = clip(
            self.read(id).await.unwrap_or_default(),
            cfg.terminal.max_output_bytes,
        );
        Ok(if printed.trim().is_empty() {
            "[sent; nothing printed yet — use terminal_read to collect the reply]".to_string()
        } else {
            printed
        })
    }

    // --- running -----------------------------------------------------------

    pub async fn run(
        &self,
        cfg: &Config,
        id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<TerminalOutput> {
        self.run_until(cfg, id, command, timeout, false, None).await
    }

    /// `run`, but giving up the wait as soon as `cancel` is raised.
    ///
    /// A stopped command is *not* killed and the shell is left alone: a session
    /// whose shell died mid-command would be useless afterwards, and a partly
    /// applied command is not made better by severing it. What changes is that
    /// nobody waits for it any longer — whatever it printed so far is returned
    /// immediately, flagged as timed out, and the shell finishes in its own
    /// time. The next `read` on the session picks up the rest.
    pub async fn run_until(
        &self,
        cfg: &Config,
        id: &str,
        command: &str,
        timeout: Duration,
        background: bool,
        cancel: Option<Arc<crate::session::CancelFlag>>,
    ) -> Result<TerminalOutput> {
        Self::require_enabled(cfg)?;

        // Whether this session is local decides two things below: whether the
        // workspace guard applies at all, and what a timeout is allowed to
        // kill.
        let (remote, pty, busy) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(id)
                .ok_or_else(|| anyhow!("no terminal session {id}"))?;
            (
                session.remote.clone(),
                session.pty,
                session.pending.clone(),
            )
        };

        // A session with a backgrounded command still running is not free to
        // take another: the shell is not reading stdin, so the new command
        // would sit in the pipe and its output would interleave with the old
        // one's. Said plainly, with the way out.
        if let Some(pending) = busy {
            return Err(anyhow!(
                "session {id} is still running a background command ({}), started {}s ago. \
                 Collect it with terminal_read, interrupt it with terminal_signal, or open \
                 another session.",
                pending.command,
                pending.started.elapsed().as_secs()
            ));
        }

        // The shell is confined at `open`, but nothing stopped it walking out
        // afterwards, and in practice that is what happened: 38% of the
        // commands three self-modifying agents ran began `cd` into the shared
        // trunk checkout, so "one worktree per conversation" stopped being
        // isolation the moment a build started. Refused up front, with the
        // reason, because the alternative is three agents in one cargo target
        // directory finding out the hard way.
        //
        // Only for a local session. On a remote host the roots name paths on
        // *this* machine, so the check would be comparing directories on two
        // different filesystems: `/srv/app` on a build box has nothing to do
        // with `/srv/app` here. There is no isolation to protect there and
        // nothing sensible to enforce, so remote sessions are exempt — which is
        // why every remote result says which host it ran on.
        if remote.is_empty() {
            if let Some(escape) = escapes_the_roots(cfg, command) {
                return Err(anyhow!(
                    "this command would leave your workspace by changing directory to {escape}. \
                     Your own checkout is {}, and it is the only tree you should build or edit — \
                     it is a full checkout of the project, not a fragment. Working in the shared \
                     one collides with the other conversations doing the same thing.",
                    cfg.filesystem
                        .roots
                        .first()
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|| cfg.root.display().to_string()),
                ));
            }
        }

        let marker = format!(
            "{MARKER_PREFIX}{}__",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        // Take what we need from the map, then let go of it. The write below
        // can block — a shell busy with a previous command is not reading
        // stdin, and a script larger than the pipe buffer parks there — and
        // holding the map lock across that froze every other terminal call,
        // including the `terminal_list` someone would use to diagnose it.
        let (buffer, display, stdin, pgid, leader, cwd, shell, user) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| anyhow!("no terminal session {id}"))?;

            if matches!(session.child.try_wait(), Ok(Some(_))) {
                return Err(anyhow!("terminal session {id} has exited; open a new one"));
            }

            // Anything left from a previous command would otherwise be reported
            // as this command's output.
            take(&session.buffer);

            session.commands += 1;
            session.last_used = Instant::now();
            (
                session.buffer.clone(),
                session.display.clone(),
                session.stdin.clone(),
                session.pgid,
                session.child.id().unwrap_or(0) as i32,
                session.cwd.clone(),
                session.shell.clone(),
                session.user.clone(),
            )
        };

        // The command itself, echoed into the watcher's transcript. A shell fed
        // from a pipe prints no prompt and no echo, so without this the drawer
        // would show output with nothing to say what produced it.
        //
        // Styled as a real prompt — `user@host:cwd $ command` — because the
        // drawer shows several shells that may sit on different machines in
        // different directories, and a bare `$` cannot tell them apart. The
        // pieces are wrapped in the marker prefix's sibling escape below rather
        // than coloured here; see `prompt_line`.
        let prompt = prompt_line(&user, &remote, &cwd, command);
        append_display(&display, &prompt);
        self.announce(id, "command", prompt, &cwd, &shell, &remote);

        {
            let script = format!("{command}\n{}\n", echo_marker(cfg, &marker));
            let mut stdin = stdin.lock().await;
            let write = async {
                stdin.write_all(script.as_bytes()).await?;
                stdin.flush().await
            };
            // Bounded by the caller's own budget: a shell that will not accept
            // the command is reported, not waited on forever.
            match tokio::time::timeout(timeout, write).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(anyhow!("cannot write to {id}: {e}")),
                Err(_) => {
                    return Err(anyhow!(
                        "terminal session {id} would not accept the command within {}s — \
                         it is wedged on a previous one. Close it and open a new session.",
                        timeout.as_secs()
                    ))
                }
            }
        }

        // Backgrounding is not a second mechanism: the command has already been
        // written exactly as a foreground one, so all that changes is who waits
        // for the marker. The session remembers it, and `read` finishes the job.
        if background {
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(id) {
                session.pending = Some(Pending {
                    marker: marker.clone(),
                    command: command.to_string(),
                    started: Instant::now(),
                });
            }
            // Give it a moment: a command that fails instantly — a typo, a
            // missing file — is far more useful reported now than discovered on
            // a later read.
            drop(sessions);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let early = take(&buffer);
            return Ok(TerminalOutput {
                output: format!(
                    "[started in the background{}]{}\n\n\
                     Collect it with terminal_read, interrupt it with terminal_signal.",
                    if remote.is_empty() {
                        String::new()
                    } else {
                        format!(" on {remote}")
                    },
                    if early.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", clip(early, cfg.terminal.max_output_bytes))
                    }
                ),
                timed_out: true,
                truncated: false,
                exit_code: None,
                host: remote.clone(),
            });
        }

        // Poll rather than hold the session lock, so other calls are not blocked
        // by a long-running command.
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((output, completion)) = split_at_marker(&buffer, &marker) {
                let truncated = output.len() > cfg.terminal.max_output_bytes;
                let mut text = clip(output, cfg.terminal.max_output_bytes);

                // The session's directory is only known here, so this is also
                // where `terminal_list` stops going stale after a `cd`.
                let mut sessions = self.sessions.lock().await;
                let mut now_at = cwd.clone();
                let note = match sessions.get_mut(id) {
                    Some(session) => {
                        // The exit status now travels as a field, so the note
                        // only has to carry what the field cannot: a move.
                        let note = completion.note(&session.cwd);
                        if let Some(cwd) = completion.cwd {
                            session.cwd = cwd.clone();
                            now_at = cwd;
                        }
                        note
                    }
                    None => String::new(),
                };
                text.push_str(&note);
                drop(sessions);

                // The exit status, for the watcher. `note` is empty for a
                // command that worked and stayed put, which is the majority —
                // the drawer wants the status either way, so it is sent as its
                // own field rather than as prose.
                let status = completion.exit_code.unwrap_or(0);
                self.announce(id, "exit", status.to_string(), &now_at, &shell, &remote);

                return Ok(TerminalOutput {
                    output: text,
                    timed_out: false,
                    truncated,
                    exit_code: completion.exit_code,
                    host: remote.clone(),
                });
            }
            // A stop is treated like the deadline arriving early: same
            // partial output, same "still running" flag, just without
            // the wait.
            let stopped = cancel.as_ref().is_some_and(|c| c.raised());
            if stopped || Instant::now() >= deadline {
                // End the runaway before returning. Otherwise it keeps
                // running, keeps writing into the buffer, and keeps the shell
                // from reading the *next* command — so every later call to this
                // session times out too, with output belonging to this one.
                // The shell itself is spared, so the session survives.
                //
                // On a remote session there is nothing local to kill: the only
                // child here is ssh itself, and killing that would take the
                // session with it. Down a pty the interrupt travels as a
                // control byte, which is the one channel there is; without a
                // pty the far side simply keeps going, and the caller is told
                // so below rather than left to infer it.
                let interrupted_remotely = if remote.is_empty() {
                    signal_children(pgid, leader, libc::SIGKILL);
                    true
                } else if pty {
                    let mut stdin = stdin.lock().await;
                    let sent = stdin.write_all(&[0x03]).await.is_ok();
                    stdin.flush().await.ok();
                    sent
                } else {
                    false
                };

                // The shell carries on to the marker line once the command it
                // was running is gone, and that marker must be consumed here:
                // left in the buffer it becomes the head of the *next*
                // command's output. Brief, because the shell has nothing left
                // to do but print it.
                let settle = Instant::now() + Duration::from_millis(750);
                let partial = loop {
                    if let Some((output, _)) = split_at_marker(&buffer, &marker) {
                        break output;
                    }
                    if Instant::now() >= settle {
                        break take(&buffer);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                };

                // Whatever arrived is still worth returning: a command that was
                // merely slow has usually printed something useful.
                let truncated = partial.len() > cfg.terminal.max_output_bytes;
                let mut output = clip(partial, cfg.terminal.max_output_bytes);
                if stopped {
                    tracing::info!(terminal = %id, "stopped waiting for a command: the turn was stopped");
                    output.push_str(
                        "\n\n[you stopped this turn; the command is still running in the shell]",
                    );
                }
                if !interrupted_remotely {
                    output.push_str(&format!(
                        "\n\n[this session runs on {remote} without a terminal, so the command \
                         could not be interrupted — it is still running there. This session \
                         will not accept another command until it ends; set pty=true on the \
                         host to make interrupts possible.]"
                    ));
                }
                // The watcher sees no marker and no exit line, so without this
                // the drawer would sit on a spinning busy dot for good.
                let why = if stopped {
                    "stopped"
                } else if !interrupted_remotely {
                    "timed out — still running on the remote host"
                } else {
                    "timed out"
                };
                let line = format!("[{why}]\n");
                append_display(&display, &line);
                self.announce(id, "output", line, &cwd, &shell, &remote);
                self.announce(id, "exit", "timeout".into(), &cwd, &shell, &remote);
                return Ok(TerminalOutput {
                    output,
                    timed_out: true,
                    truncated,
                    exit_code: None,
                    host: remote.clone(),
                });
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    /// Drops sessions whose shell has exited or that have gone idle.
    fn reap(&self, sessions: &mut HashMap<String, Session>, cfg: &Config) {
        let idle = cfg.terminal.idle_timeout;
        sessions.retain(|id, session| {
            let exited = matches!(session.child.try_wait(), Ok(Some(_)));
            let stale = idle > Duration::ZERO && session.last_used.elapsed() > idle;
            if exited || stale {
                tracing::debug!(terminal = %id, exited, stale, "reaping terminal session");
                // `kill_on_drop` only reaches the shell. Anything it started
                // would survive holding its pipes — and, until fd 3 became
                // close-on-exec, the gateway socket itself.
                signal_group(session.pgid, libc::SIGKILL);
                // A tab with the drawer open would otherwise keep a tab for a
                // shell that no longer exists, forever.
                let _ = self.feed_tx.send(TerminalFeed {
                    id: id.clone(),
                    kind: "closed".into(),
                    text: String::new(),
                    cwd: session.cwd.clone(),
                    shell: session.shell.clone(),
                    remote: session.remote.clone(),
                });
                return false;
            }
            true
        });
    }

    /// Kills every session, for shutdown.
    pub async fn close_all(&self) {
        let mut sessions = self.sessions.lock().await;
        for (id, mut session) in sessions.drain() {
            session.terminate().await;
            let _ = self.feed_tx.send(TerminalFeed {
                id,
                kind: "closed".into(),
                text: String::new(),
                cwd: session.cwd.clone(),
                shell: session.shell.clone(),
                remote: session.remote.clone(),
            });
        }
    }
}

/// The first `cd` target in `command` that lands outside the configured roots.
///
/// Deliberately a text check on the command rather than a check after the
/// fact: by the time the shell reports its new directory the build has already
/// run in the wrong tree. Only absolute targets are judged — a relative `cd`
/// cannot escape a root without `..`, which `hostfs::resolve` already refuses.
fn escapes_the_roots(cfg: &Config, command: &str) -> Option<String> {
    for raw in command.split(|c| c == ';' || c == '&' || c == '|' || c == '\n') {
        let mut words = raw.split_whitespace();
        if words.next().map(str::trim) != Some("cd") {
            continue;
        }
        let Some(target) = words.next() else { continue };
        // `cd -`, `cd ~`, `cd $VAR`: not statically knowable, and none of them
        // is the pattern this is here to stop.
        let target = target.trim_matches(|c| c == '"' || c == '\'');
        if !target.starts_with('/') {
            continue;
        }
        if crate::hostfs::resolve(cfg, target).is_err() {
            return Some(target.to_string());
        }
    }
    None
}

// --- helpers ----------------------------------------------------------------

/// Echoes the marker in a way the configured shell understands.
/// The line a shell prints to say a command has finished.
///
/// It carries the exit status and the working directory as well as the marker,
/// because both are expanded by the shell at the moment the command ends and
/// there is no other way to ask for them afterwards. Without this the agent has
/// no way to tell a command that worked from one that failed silently, which is
/// why so many of its command lines used to end in `&& echo ok`.
fn echo_marker(cfg: &Config, marker: &str) -> String {
    let shell = cfg.terminal.shell.to_lowercase();
    if shell.contains("powershell") || shell.contains("pwsh") {
        // Write-Output rather than echo: it is not aliased away by a profile.
        // `$LASTEXITCODE` is unset until a native command has run, and an empty
        // field simply reads as "unknown" on the other side.
        format!("Write-Output \"{marker}`t$LASTEXITCODE`t$PWD\"")
    } else {
        // The arguments are expanded before `printf` runs, so `$?` is still the
        // status of the command the caller asked for.
        format!("printf '%s\\t%s\\t%s\\n' '{marker}' \"$?\" \"$PWD\"")
    }
}

/// What a completion marker tells us besides "the command is over".
#[derive(Default)]
struct Completion {
    exit_code: Option<i32>,
    cwd: Option<String>,
}

impl Completion {
    /// Parses the tab-separated tail of the marker line.
    ///
    /// The tail still carries the separator that followed the marker, so the
    /// first field is always empty and is skipped.
    ///
    /// Anything unparseable is left as `None` rather than guessed: reporting a
    /// wrong exit status would be worse than reporting none.
    fn parse(trailer: &str) -> Self {
        let mut fields = trailer.trim_end_matches('\r').split('\t').skip(1);
        Self {
            exit_code: fields.next().and_then(|f| f.trim().parse::<i32>().ok()),
            cwd: fields
                .next()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
        }
    }

    /// The note appended to the command's output, empty when there is nothing
    /// worth saying.
    ///
    /// The exit status used to be reported here as text. It travels as a field
    /// now, so the note is left with only what a field cannot carry: that the
    /// session has moved. A successful command that stayed put says nothing.
    fn note(&self, previous_cwd: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(cwd) = self.cwd.as_deref().filter(|c| *c != previous_cwd) {
            parts.push(format!("working directory is now {cwd}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("\n\n[{}]", parts.join(" · "))
        }
    }
}

/// Where a pump tees output for anyone watching the session.
#[derive(Clone)]
struct Watch {
    display: Arc<Mutex<String>>,
    feed: tokio::sync::broadcast::Sender<TerminalFeed>,
    id: String,
}

/// Appends to a transcript, keeping only its tail.
fn append_display(display: &Arc<Mutex<String>>, text: &str) {
    let Ok(mut buf) = display.lock() else { return };
    buf.push_str(text);
    if buf.len() > MAX_DISPLAY_BYTES * 2 {
        let mut cut = buf.len() - MAX_DISPLAY_BYTES;
        while cut < buf.len() && !buf.is_char_boundary(cut) {
            cut += 1;
        }
        *buf = buf.split_off(cut);
    }
}

/// Reads lines from a stream into the shared buffer.
fn pump<R>(stream: R, buffer: Arc<Mutex<String>>, cap: usize, watch: Watch)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        // Bytes, not `lines()`: a single non-UTF-8 byte — one `cat` of a
        // binary file, one program printing latin-1 — made `next_line` return
        // `Err(InvalidData)`, and the old `while let Ok(Some(_))` treated that
        // exactly like end-of-stream. The pump died silently and every later
        // command in that session timed out with no output, forever.
        let mut reader = BufReader::new(stream);
        let mut raw: Vec<u8> = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "terminal output stream ended");
                    return;
                }
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.strip_suffix('\n').unwrap_or(&line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            {
                let Ok(mut buf) = buffer.lock() else { return };
                buf.push_str(line);
                buf.push('\n');
                // Keep the tail: a runaway command must not grow this without
                // bound.
                if buf.len() > cap * 4 {
                    let keep = buf.len() - cap * 2;
                    *buf = buf.split_off(keep);
                }
            }
            // The tee for anyone watching. The completion marker is protocol,
            // not output — it carries the exit status and the working directory
            // — so it is filtered here rather than shown to a human as a line
            // of gibberish after every command.
            if line.contains(MARKER_PREFIX) {
                continue;
            }
            let text = indent_output(line);
            append_display(&watch.display, &text);
            // cwd, shell and remote are left empty on an output event: the
            // drawer already has them from `opened`, and the pump would have to
            // take the sessions lock on every line to restate them.
            let _ = watch.feed.send(TerminalFeed {
                id: watch.id.clone(),
                kind: "output".into(),
                text,
                cwd: String::new(),
                shell: String::new(),
                remote: String::new(),
            });
        }
    });
}

/// The prompt line the drawer shows above a command's output.
///
/// A shell fed from a pipe never prints a prompt, so this is synthesised. It is
/// coloured with SGR escapes rather than styled in CSS because the transcript
/// is a single stream of text inside the emulator — there is no DOM node per
/// line to attach a class to.
///
/// The command is written last and never coloured, so a command containing its
/// own escapes cannot leak style into the prompt that follows it: everything is
/// reset before the command begins.
fn prompt_line(user: &str, remote: &str, cwd: &str, command: &str) -> String {
    let mut out = String::from("\r\n\x1b[38;5;71m");
    if user.is_empty() {
        out.push_str("shell");
    } else {
        out.push_str(user);
    }
    // A local shell says the directory only; a remote one has to name the
    // machine, since two tabs may otherwise look identical.
    if !remote.is_empty() {
        out.push('@');
        out.push_str(remote);
    }
    out.push_str("\x1b[0m:\x1b[38;5;110m");
    out.push_str(&contract_home(cwd));
    out.push_str("\x1b[0m\x1b[38;5;245m $ \x1b[0m");
    // A multi-line command would break the indent of the output that follows,
    // so continuation lines are folded onto one with a visible marker.
    let flat = command.trim_end();
    if flat.contains('\n') {
        let joined: Vec<&str> = flat.lines().map(str::trim).collect();
        out.push_str(&joined.join(" \x1b[38;5;245m⏎\x1b[0m "));
    } else {
        out.push_str(flat);
    }
    out.push_str("\r\n");
    out
}

/// Replaces the home directory with `~`, as a shell prompt does. Keeps a deep
/// path from pushing the `$` off the end of a narrow drawer.
fn contract_home(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && cwd.starts_with(&home) {
        return format!("~{}", &cwd[home.len()..]);
    }
    cwd.to_string()
}

/// Indents one line of command output so it reads as subordinate to the prompt
/// above it.
///
/// The indent is written after any escape the line begins with, and `\r` is
/// emitted first, because a program that returns the carriage to redraw a
/// progress line would otherwise overwrite the indent and leave the line
/// hanging two columns left of its neighbours.
fn indent_output(line: &str) -> String {
    if line.is_empty() {
        return "\r\n".to_string();
    }
    format!("\r  {line}\r\n")
}

fn take(buffer: &Arc<Mutex<String>>) -> String {
    match buffer.lock() {
        Ok(mut buf) => std::mem::take(&mut *buf),
        Err(_) => String::new(),
    }
}

/// Returns the output before the marker once it has arrived, consuming it.
fn split_at_marker(buffer: &Arc<Mutex<String>>, marker: &str) -> Option<(String, Completion)> {
    let mut buf = buffer.lock().ok()?;

    // The command that echoes the marker is itself echoed by some shells, so
    // match the last occurrence: that one is the real completion.
    let at = buf.rfind(marker)?;
    let before = buf[..at].to_string();
    let after = buf[at + marker.len()..].to_string();

    // The status and directory ride on the rest of the marker's own line, so
    // that line is consumed here rather than left to surface as stray output on
    // the next read.
    let (trailer, rest) = match after.split_once('\n') {
        Some((trailer, rest)) => (trailer.to_string(), rest.to_string()),
        None => (after, String::new()),
    };
    *buf = rest.trim_start_matches('\n').to_string();

    // Drop the echoed command line that produced the marker, if present.
    let cleaned: Vec<&str> = before
        .lines()
        .filter(|line| !line.contains(marker))
        .collect();
    Some((
        cleaned.join("\n").trim_end().to_string(),
        Completion::parse(&trailer),
    ))
}

fn clip(text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    // Keep the tail: the end of a command's output is usually the part that
    // says what happened.
    let mut cut = text.len() - cap;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[earlier output trimmed]\n{}", &text[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_of(text: &str) -> Arc<Mutex<String>> {
        Arc::new(Mutex::new(text.to_string()))
    }

    /// Strips SGR escapes, so a test can assert on what a human reads rather
    /// than on the colour codes wrapped around it.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Consume through the terminating 'm' of a CSI sequence.
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn the_prompt_line_names_the_user_and_the_directory() {
        let line = prompt_line("bitmuse", "", "/srv/app", "cargo build");
        assert_eq!(plain(&line), "\r\nbitmuse:/srv/app $ cargo build\r\n");
        // The command is written after a reset, so a command containing its own
        // escapes cannot leak style into what follows.
        assert!(line.ends_with("\x1b[0mcargo build\r\n"));
    }

    #[test]
    fn a_remote_prompt_names_the_machine() {
        let line = prompt_line("build", "buildbox", "/srv/app", "make");
        assert_eq!(plain(&line), "\r\nbuild@buildbox:/srv/app $ make\r\n");
    }

    #[test]
    fn a_missing_user_still_produces_a_usable_prompt() {
        let line = prompt_line("", "", "/tmp", "ls");
        assert_eq!(plain(&line), "\r\nshell:/tmp $ ls\r\n");
    }

    #[test]
    fn a_multi_line_command_is_folded_onto_one_prompt_line() {
        // Otherwise the continuation lines land at column zero and break the
        // indent of the output beneath them.
        let line = prompt_line("u", "", "/tmp", "for f in *; do\n  echo $f\ndone");
        let text = plain(&line);
        assert_eq!(text.matches('\n').count(), 2, "only the wrapping newlines: {text:?}");
        assert!(text.contains("for f in *; do ⏎ echo $f ⏎ done"), "{text:?}");
    }

    #[test]
    fn output_is_indented_under_its_prompt() {
        assert_eq!(indent_output("Compiling thetis"), "\r  Compiling thetis\r\n");
        // A blank line stays blank rather than becoming two stray spaces.
        assert_eq!(indent_output(""), "\r\n");
    }

    #[test]
    fn the_indent_survives_a_carriage_return_redraw() {
        // A progress bar rewrites its line with \r. The indent is re-emitted
        // after it, so the redrawn line does not end up two columns left of
        // its neighbours.
        let out = indent_output("\rDownloading 50%");
        assert!(out.starts_with("\r  "), "{out:?}");
    }

    #[test]
    fn marker_absent_means_the_command_is_still_running() {
        let buf = buffer_of("building...\n");
        assert!(split_at_marker(&buf, "__thetis_done_1__").is_none());
        // Nothing is consumed while waiting.
        assert_eq!(buf.lock().unwrap().as_str(), "building...\n");
    }

    #[test]
    fn output_before_the_marker_is_returned_and_consumed() {
        let buf = buffer_of("line one\nline two\n__thetis_done_1__\t0\t/tmp\nleftover\n");
        let (output, done) = split_at_marker(&buf, "__thetis_done_1__").unwrap();
        assert_eq!(output, "line one\nline two");
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(done.cwd.as_deref(), Some("/tmp"));
        // What arrived after belongs to whatever comes next, and the status
        // fields are consumed rather than left to look like stray output.
        assert_eq!(buf.lock().unwrap().as_str(), "leftover\n");
    }

    #[test]
    fn an_echoed_command_line_does_not_end_the_command_early() {
        // Some shells echo the line that will print the marker; the real
        // completion is the last occurrence, not the first.
        let buf = buffer_of(
            "Write-Output '__thetis_done_2__'\nreal output\n__thetis_done_2__\t0\t/tmp\n",
        );
        let (output, _) = split_at_marker(&buf, "__thetis_done_2__").unwrap();
        assert_eq!(output, "real output");
    }

    #[test]
    fn a_cd_is_reported_once_and_a_status_never_is() {
        // The status travels as a field on the result, so the note must not
        // narrate it as well — that was the old behaviour and it left callers
        // parsing prose to tell success from failure.
        let failed = Completion {
            exit_code: Some(2),
            cwd: Some("/home/x".into()),
        };
        assert_eq!(failed.note("/home/x"), "");

        let moved = Completion {
            exit_code: Some(0),
            cwd: Some("/home/x/src".into()),
        };
        assert_eq!(
            moved.note("/home/x"),
            "\n\n[working directory is now /home/x/src]"
        );

        // A command that worked and stayed put has nothing to add.
        let quiet = Completion {
            exit_code: Some(0),
            cwd: Some("/home/x".into()),
        };
        assert_eq!(quiet.note("/home/x"), "");
    }

    #[test]
    fn an_unreadable_status_is_reported_as_nothing_rather_than_guessed() {
        // PowerShell leaves `$LASTEXITCODE` unset until a native command runs.
        let done = Completion::parse("\t\t/tmp");
        assert_eq!(done.exit_code, None);
        assert_eq!(done.note("/elsewhere"), "\n\n[working directory is now /tmp]");
    }

    #[test]
    fn output_is_clipped_from_the_front_keeping_the_end() {
        let text = format!("{}IMPORTANT TAIL", "x".repeat(500));
        let clipped = clip(text, 50);
        assert!(clipped.ends_with("IMPORTANT TAIL"), "{clipped}");
        assert!(clipped.starts_with("[earlier output trimmed]"));
    }

    #[test]
    fn short_output_is_left_alone() {
        assert_eq!(clip("done".to_string(), 100), "done");
    }

    /// Drives a real shell, because everything above only proves the parser
    /// agrees with itself — the thing that actually has to be true is that the
    /// shell prints what we think we asked it for.
    /// A single non-UTF-8 byte used to kill the output pump for good: the
    /// `while let Ok(Some(line))` loop treated `Err(InvalidData)` exactly like
    /// end-of-stream, so every later command in the session timed out with no
    /// output and the terminal was silently dead.
    #[tokio::test]
    async fn a_command_that_leaves_the_workspace_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();
        let wait = Duration::from_secs(10);

        // The exact shape three agents used to reach the shared checkout.
        let err = terminals
            .run(&cfg, &id, "cd /usr/share && cargo build --release", wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("leave your workspace"), "{err}");
        assert!(err.contains("/usr/share"), "it must name where it would have gone: {err}");

        // The session survives the refusal and still works.
        let ok = terminals.run(&cfg, &id, "echo still-usable", wait).await.unwrap();
        assert_eq!(ok.output, "still-usable");

        // Moving around *inside* the workspace is untouched.
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let moved = terminals.run(&cfg, &id, "cd sub && pwd", wait).await.unwrap();
        assert!(moved.output.contains("sub"), "{moved:?}");
    }

    #[tokio::test]
    async fn a_binary_byte_does_not_kill_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();
        let wait = Duration::from_secs(10);

        let binary = terminals
            .run(&cfg, &id, r"printf '\x80\xff\n'", wait)
            .await
            .unwrap();
        assert!(!binary.timed_out, "the binary write itself must complete");

        let after = terminals.run(&cfg, &id, "echo still-here", wait).await.unwrap();
        assert_eq!(
            after.output, "still-here",
            "the pump must survive invalid UTF-8: {after:?}"
        );
    }

    /// A command that outlives its timeout used to be left running, so it went
    /// on holding the shell and every later command in the session timed out
    /// too — with the *previous* command's output. The runaway is interrupted
    /// on the way out.
    #[tokio::test]
    async fn a_timed_out_command_does_not_wedge_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        let slow = terminals
            .run(&cfg, &id, "sleep 30", Duration::from_secs(2))
            .await
            .unwrap();
        assert!(slow.timed_out, "the sleep must outlast its budget");

        let after = terminals
            .run(&cfg, &id, "echo recovered", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            after.output, "recovered",
            "the session must accept the next command: {after:?}"
        );
    }

    /// Closing a session must take everything it started with it. A background
    /// process that survives holds its pipes — and, before fd 3 became
    /// close-on-exec, the gateway socket, which made the whole conversation
    /// unreachable until the gateway restarted.
    #[tokio::test]
    async fn closing_a_session_kills_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();
        let wait = Duration::from_secs(10);

        let pid = terminals
            .run(&cfg, &id, "sleep 300 & echo $!", wait)
            .await
            .unwrap()
            .output
            .trim()
            .parse::<i32>()
            .expect("the shell printed a pid");
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the background process should be running"
        );

        terminals.close(&id).await.unwrap();
        // Signals are asynchronous; give the group a moment to go.
        for _ in 0..50 {
            if !std::path::Path::new(&format!("/proc/{pid}/cmdline")).exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A zombie is fine — it is reaped by init and holds nothing open.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        assert!(
            status.contains("zombie") || status.is_empty(),
            "the background process outlived its shell: {status}"
        );
    }

    #[tokio::test]
    async fn a_real_shell_reports_its_exit_status_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inner")).unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();
        let wait = Duration::from_secs(10);

        let ok = terminals.run(&cfg, &id, "echo hello", wait).await.unwrap();
        assert_eq!(ok.output, "hello", "a working command says nothing extra");

        // A command that fails rather than `exit`, which would end the session.
        // The status is a field now, not a line of prose in the output.
        let failed = terminals
            .run(&cfg, &id, "ls /definitely/not/here", wait)
            .await
            .unwrap();
        assert!(
            matches!(failed.exit_code, Some(code) if code != 0),
            "a failing command must report its status: {failed:?}"
        );

        let moved = terminals.run(&cfg, &id, "cd inner", wait).await.unwrap();
        assert!(
            moved.output.contains("working directory is now"),
            "{:?}",
            moved.output
        );
        // And the session's own record of where it is keeps up, so a later
        // listing is not lying.
        let listed = terminals.list().await;
        assert!(
            listed.iter().any(|s| s.cwd.ends_with("inner")),
            "{:?}",
            listed.iter().map(|s| &s.cwd).collect::<Vec<_>>()
        );

        terminals.close_all().await;
    }

    /// The reported bug, at the level that actually reproduces it: a long
    /// command, a stop while it runs, and a call that has to come back promptly
    /// rather than waiting out its timeout.
    #[tokio::test]
    async fn stopping_a_turn_abandons_a_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        let cancel = Arc::new(crate::session::CancelFlag::default());
        cancel.begin_turn();

        // Raise the stop shortly after the command starts.
        let raiser = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                cancel.raise();
            })
        };

        let started = Instant::now();
        // A generous timeout: the point is that the stop, not the deadline, is
        // what ends the wait.
        let out = terminals
            .run_until(
                &cfg,
                &id,
                "echo starting; sleep 30",
                Duration::from_secs(30),
                false,
                Some(cancel),
            )
            .await
            .unwrap();
        let waited = started.elapsed();
        raiser.await.unwrap();

        assert!(
            waited < Duration::from_secs(5),
            "the stop must cut the wait short, but it took {waited:?}"
        );
        assert!(out.timed_out, "the command did not finish");
        assert!(
            out.output.contains("you stopped this turn"),
            "the guest should be told why it came back early: {:?}",
            out.output
        );
        // Output from before the stop is not thrown away.
        assert!(
            out.output.contains("starting"),
            "partial output is still worth having: {:?}",
            out.output
        );

        terminals.close_all().await;
    }

    #[tokio::test]
    async fn a_command_that_finishes_first_is_unaffected_by_a_later_stop() {
        // The stop must not cost a result that was already in hand.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        let cancel = Arc::new(crate::session::CancelFlag::default());
        cancel.begin_turn();

        let out = terminals
            .run_until(
                &cfg,
                &id,
                "echo done",
                Duration::from_secs(10),
                false,
                Some(cancel.clone()),
            )
            .await
            .unwrap();

        assert_eq!(out.output, "done");
        assert!(!out.timed_out);

        // Stopping afterwards changes nothing about what was returned.
        cancel.raise();
        assert_eq!(out.output, "done");

        terminals.close_all().await;
    }

    #[tokio::test]
    async fn a_stop_raised_before_the_command_returns_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];

        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        let cancel = Arc::new(crate::session::CancelFlag::default());
        cancel.begin_turn();
        cancel.raise();

        let started = Instant::now();
        let out = terminals
            .run_until(
                &cfg,
                &id,
                "sleep 30",
                Duration::from_secs(30),
                false,
                Some(cancel),
            )
            .await
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an already-stopped turn must not wait at all"
        );
        assert!(out.timed_out);

        terminals.close_all().await;
    }

    /// A test config with one root and the terminal on. Every session test
    /// needs it, and the tempdir must be kept alive by the caller.
    fn test_config(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::load().unwrap();
        cfg.terminal.enabled = true;
        cfg.filesystem.enabled = true;
        cfg.filesystem.roots = vec![dir.path().canonicalize().unwrap()];
        cfg
    }

    /// The exit status is what tells success from failure, and it used to be
    /// prose in the output that a caller had to parse. It is a field now, so
    /// assert both that it arrives and that a zero is distinguishable from
    /// "the shell never said".
    #[tokio::test]
    async fn the_exit_status_comes_back_as_a_field() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();
        let wait = Duration::from_secs(10);

        let ok = terminals.run(&cfg, &id, "echo fine", wait).await.unwrap();
        assert_eq!(ok.exit_code, Some(0), "{ok:?}");
        assert_eq!(ok.output, "fine");
        assert!(
            !ok.output.contains("exit status"),
            "the status must not also be narrated into the output: {ok:?}"
        );

        // A subshell: a bare `exit 3` would end the session's own shell.
        let bad = terminals.run(&cfg, &id, "(exit 3)", wait).await.unwrap();
        assert_eq!(bad.exit_code, Some(3), "{bad:?}");

        // A local session must never claim to have run somewhere else.
        assert!(ok.host.is_empty(), "{ok:?}");

        terminals.close_all().await;
    }

    /// Background mode exists so a long command does not have to be waited on.
    /// The three things that must hold: it returns fast, the session refuses a
    /// second command while it runs, and a later read reports the finish.
    #[tokio::test]
    async fn a_background_command_returns_at_once_and_is_collected_later() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        let started = Instant::now();
        let out = terminals
            .run_until(
                &cfg,
                &id,
                "sleep 2; echo finished-later",
                Duration::from_secs(30),
                true,
                None,
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "backgrounding must not wait for the command: took {:?}",
            started.elapsed()
        );
        assert!(
            out.output.contains("started in the background"),
            "it must say what it did: {out:?}"
        );

        // The shell is not reading stdin, so a second command cannot be
        // interleaved — and the refusal has to explain the way out.
        let err = terminals
            .run(&cfg, &id, "echo intruder", Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("background command"), "{err}");
        assert!(err.contains("terminal_read"), "{err}");

        // Once it ends, a read reports the completion and the output.
        let mut collected = String::new();
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            collected = terminals.read(&id).await.unwrap();
            if collected.contains("the background command finished") {
                break;
            }
        }
        assert!(
            collected.contains("finished-later"),
            "the output must survive the wait: {collected:?}"
        );
        assert!(
            collected.contains("the background command finished"),
            "the read must say the command ended: {collected:?}"
        );

        // And the session is free again.
        let after = terminals
            .run(&cfg, &id, "echo free", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(after.output, "free");

        terminals.close_all().await;
    }

    /// Interrupting must end the command and spare the shell. Before this the
    /// only way to stop a runaway was a timeout, which killed its children as
    /// a side effect rather than on purpose.
    #[tokio::test]
    async fn a_signal_ends_the_command_but_not_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        terminals
            .run_until(
                &cfg,
                &id,
                "sleep 300",
                Duration::from_secs(30),
                true,
                None,
            )
            .await
            .unwrap();

        let note = terminals.signal(&cfg, &id, "INT").await.unwrap();
        assert!(note.contains("SIGINT"), "{note}");

        // The session survives and is usable, which is the whole point.
        let mut usable = None;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let _ = terminals.read(&id).await;
            if let Ok(out) = terminals
                .run(&cfg, &id, "echo alive", Duration::from_secs(5))
                .await
            {
                usable = Some(out);
                break;
            }
        }
        let out = usable.expect("the shell must outlive the interrupt");
        assert!(out.output.contains("alive"), "{out:?}");

        // `sig` spelt with the prefix is the same signal, not an error.
        assert!(terminals.signal(&cfg, &id, "sigint").await.is_ok());

        terminals.close_all().await;
    }

    /// `send` is the escape hatch from the marker protocol: a program waiting
    /// at a prompt never completes, so `run` can never drive one.
    #[tokio::test]
    async fn raw_input_can_answer_a_prompt_that_run_could_never_reach() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();
        let id = terminals.open(&cfg, OpenSpec::local()).await.unwrap();

        // A read that will sit there forever as far as `run` is concerned.
        terminals
            .run_until(
                &cfg,
                &id,
                "read -r answer; echo \"you said: $answer\"",
                Duration::from_secs(30),
                true,
                None,
            )
            .await
            .unwrap();

        // `send` returns what came back in the settle window, so the reply is
        // often already in hand — start the collection with it rather than
        // discarding it and waiting for a read that has nothing left to give.
        let mut collected = terminals
            .send(&cfg, &id, "hello-there", true, Duration::from_millis(400))
            .await
            .unwrap();

        for _ in 0..20 {
            if collected.contains("you said") {
                break;
            }
            collected.push_str(&terminals.read(&id).await.unwrap());
            if collected.contains("you said") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(
            collected.contains("you said: hello-there"),
            "the prompt must have received the input: {collected:?}"
        );

        terminals.close_all().await;
    }

    /// A label and environment variables at open, so a listing is readable and
    /// a session does not need an `export` as its first command.
    #[tokio::test]
    async fn a_session_carries_its_name_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();

        let id = terminals
            .open(
                &cfg,
                OpenSpec {
                    name: Some("build box".into()),
                    env: vec![("THETIS_TEST_VAR".into(), "carried".into())],
                    ..OpenSpec::local()
                },
            )
            .await
            .unwrap();

        let out = terminals
            .run(&cfg, &id, "echo \"$THETIS_TEST_VAR\"", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.output, "carried", "{out:?}");

        let listed = terminals.list().await;
        let info = listed.iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.name, "build box");
        assert!(info.remote.is_empty(), "a local session is not remote");
        assert!(info.busy.is_empty(), "nothing is backgrounded: {:?}", info.busy);

        terminals.close_all().await;
    }

    /// An env name the shell could not hold is a mistake worth naming, not
    /// something to pass to the kernel and hope.
    #[tokio::test]
    async fn a_malformed_environment_variable_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();

        let err = terminals
            .open(
                &cfg,
                OpenSpec {
                    env: vec![("BAD=NAME".into(), "x".into())],
                    ..OpenSpec::local()
                },
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("BAD=NAME"), "it must name the offender: {err}");
    }

    /// Remote sessions are only reachable through the registry, so an unknown
    /// name must say so — and say where names come from.
    #[tokio::test]
    async fn an_unknown_ssh_host_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir);
        let terminals = Terminals::new();

        let err = terminals
            .open(
                &cfg,
                OpenSpec {
                    host: Some("nowhere-at-all".into()),
                    ..OpenSpec::local()
                },
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nowhere-at-all"), "{err}");
    }

    #[test]
    fn the_marker_echo_matches_the_shell() {
        let mut cfg = Config::load().unwrap();
        cfg.terminal.shell = "powershell".into();
        assert!(echo_marker(&cfg, "M").starts_with("Write-Output"));
        cfg.terminal.shell = "sh".into();
        assert!(echo_marker(&cfg, "M").starts_with("printf"));
    }
}
