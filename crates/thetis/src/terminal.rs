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

struct Session {
    id: String,
    cwd: String,
    shell: String,
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
    commands: u32,
    last_used: Instant,
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

/// Kills everything in the shell's process group *except the shell*.
///
/// Signalling the group as a whole is wrong for a command timeout: a
/// non-interactive shell does not ignore SIGINT, so it dies along with the
/// runaway and the session is lost — which is exactly what the caller was
/// trying to avoid. Killing only the descendants ends the stuck command and
/// leaves the shell to read the next one.
fn kill_group_children(pgid: i32, leader: i32) {
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
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[derive(Default)]
pub struct Terminals {
    sessions: tokio::sync::Mutex<HashMap<String, Session>>,
    counter: std::sync::atomic::AtomicU64,
}

impl Terminals {
    pub fn new() -> Self {
        Self::default()
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

    pub async fn open(&self, cfg: &Config, cwd: Option<&str>) -> Result<String> {
        Self::require_enabled(cfg)?;

        let mut sessions = self.sessions.lock().await;
        self.reap(&mut sessions, cfg);

        if sessions.len() >= cfg.terminal.max_sessions {
            return Err(anyhow!(
                "already at the limit of {} terminal sessions; close one first",
                cfg.terminal.max_sessions
            ));
        }

        // The working directory goes through the same confinement as the
        // filesystem tools, so a session cannot start outside the roots.
        let dir = match cwd {
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

        let mut command = Command::new(&cfg.terminal.shell);
        command
            .args(&cfg.terminal.shell_args)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
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
        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("cannot start {}: {e}", cfg.terminal.shell))?;

        let pgid = child.id().unwrap_or(0) as i32;
        let stdin = Arc::new(tokio::sync::Mutex::new(
            child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?,
        ));
        let buffer = Arc::new(Mutex::new(String::new()));

        // Both streams feed one buffer, so interleaved output reads the way it
        // would in a real terminal.
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, buffer.clone(), cfg.terminal.max_output_bytes);
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, buffer.clone(), cfg.terminal.max_output_bytes);
        }

        let id = format!(
            "term-{}",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1
        );

        sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                cwd: dir.display().to_string(),
                shell: cfg.terminal.shell.clone(),
                child,
                stdin,
                buffer,
                pgid,
                commands: 0,
                last_used: Instant::now(),
            },
        );

        tracing::info!(terminal = %id, dir = %dir.display(), "terminal session opened");
        Ok(id)
    }

    pub async fn close(&self, id: &str) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let Some(mut session) = sessions.remove(id) else {
            return Err(anyhow!("no terminal session {id}"));
        };
        session.terminate().await;
        tracing::info!(terminal = %id, "terminal session closed");
        Ok(format!("closed {id}"))
    }

    pub async fn list(&self) -> Vec<TerminalInfo> {
        let mut sessions = self.sessions.lock().await;
        let mut out: Vec<TerminalInfo> = sessions
            .values_mut()
            .map(|s| TerminalInfo {
                id: s.id.clone(),
                cwd: s.cwd.clone(),
                shell: s.shell.clone(),
                // `try_wait` reports without blocking; `Some` means it exited.
                alive: matches!(s.child.try_wait(), Ok(None)),
                commands: s.commands,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Returns and clears whatever the shell has written since the last read.
    pub async fn read(&self, id: &str) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("no terminal session {id}"))?;
        session.last_used = Instant::now();
        Ok(take(&session.buffer))
    }

    // --- running -----------------------------------------------------------

    pub async fn run(
        &self,
        cfg: &Config,
        id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<TerminalOutput> {
        self.run_until(cfg, id, command, timeout, None).await
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
        cancel: Option<Arc<crate::session::CancelFlag>>,
    ) -> Result<TerminalOutput> {
        Self::require_enabled(cfg)?;

        // The shell is confined at `open`, but nothing stopped it walking out
        // afterwards, and in practice that is what happened: 38% of the
        // commands three self-modifying agents ran began `cd` into the shared
        // trunk checkout, so "one worktree per conversation" stopped being
        // isolation the moment a build started. Refused up front, with the
        // reason, because the alternative is three agents in one cargo target
        // directory finding out the hard way.
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

        let marker = format!(
            "__thetis_done_{}__",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        // Take what we need from the map, then let go of it. The write below
        // can block — a shell busy with a previous command is not reading
        // stdin, and a script larger than the pipe buffer parks there — and
        // holding the map lock across that froze every other terminal call,
        // including the `terminal_list` someone would use to diagnose it.
        let (buffer, stdin, pgid, leader) = {
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
                session.stdin.clone(),
                session.pgid,
                session.child.id().unwrap_or(0) as i32,
            )
        };

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
                let note = match sessions.get_mut(id) {
                    Some(session) => {
                        let note = completion.note(&session.cwd);
                        if let Some(cwd) = completion.cwd {
                            session.cwd = cwd;
                        }
                        note
                    }
                    None => String::new(),
                };
                text.push_str(&note);

                return Ok(TerminalOutput {
                    output: text,
                    timed_out: false,
                    truncated,
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
                kill_group_children(pgid, leader);

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
                return Ok(TerminalOutput {
                    output,
                    timed_out: true,
                    truncated,
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
                return false;
            }
            true
        });
    }

    /// Kills every session, for shutdown.
    pub async fn close_all(&self) {
        let mut sessions = self.sessions.lock().await;
        for (_, mut session) in sessions.drain() {
            session.terminate().await;
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
    /// worth saying. A successful command that stayed put says nothing at all.
    fn note(&self, previous_cwd: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(code) = self.exit_code.filter(|c| *c != 0) {
            parts.push(format!("exit status {code}"));
        }
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

/// Reads lines from a stream into the shared buffer.
fn pump<R>(stream: R, buffer: Arc<Mutex<String>>, cap: usize)
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
            let Ok(mut buf) = buffer.lock() else { return };
            buf.push_str(line);
            buf.push('\n');
            // Keep the tail: a runaway command must not grow this without bound.
            if buf.len() > cap * 4 {
                let keep = buf.len() - cap * 2;
                *buf = buf.split_off(keep);
            }
        }
    });
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
    fn a_failing_command_says_so_and_a_cd_is_reported_once() {
        let failed = Completion {
            exit_code: Some(2),
            cwd: Some("/home/x".into()),
        };
        let note = failed.note("/home/x");
        assert_eq!(note, "\n\n[exit status 2]");

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
        let id = terminals.open(&cfg, None).await.unwrap();
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
        let id = terminals.open(&cfg, None).await.unwrap();
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
        let id = terminals.open(&cfg, None).await.unwrap();

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
        let id = terminals.open(&cfg, None).await.unwrap();
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
        let id = terminals.open(&cfg, None).await.unwrap();
        let wait = Duration::from_secs(10);

        let ok = terminals.run(&cfg, &id, "echo hello", wait).await.unwrap();
        assert_eq!(ok.output, "hello", "a working command says nothing extra");

        // A command that fails rather than `exit`, which would end the session.
        let failed = terminals
            .run(&cfg, &id, "ls /definitely/not/here", wait)
            .await
            .unwrap();
        assert!(
            failed.output.contains("[exit status "),
            "a failing command must say so: {:?}",
            failed.output
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
        let id = terminals.open(&cfg, None).await.unwrap();

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
        let id = terminals.open(&cfg, None).await.unwrap();

        let cancel = Arc::new(crate::session::CancelFlag::default());
        cancel.begin_turn();

        let out = terminals
            .run_until(
                &cfg,
                &id,
                "echo done",
                Duration::from_secs(10),
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
        let id = terminals.open(&cfg, None).await.unwrap();

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

    #[test]
    fn the_marker_echo_matches_the_shell() {
        let mut cfg = Config::load().unwrap();
        cfg.terminal.shell = "powershell".into();
        assert!(echo_marker(&cfg, "M").starts_with("Write-Output"));
        cfg.terminal.shell = "sh".into();
        assert!(echo_marker(&cfg, "M").starts_with("printf"));
    }
}
