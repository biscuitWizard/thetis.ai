use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
fn read_password_from_tty() -> Result<String> {
    use std::io::{BufRead, Write};
    use std::os::fd::AsRawFd;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    let fd = tty.as_raw_fd();
    let mut old = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut hidden = old;
    hidden.c_lflag &= !libc::ECHO;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut writer = &tty;
    write!(writer, "Password: ")?;
    writer.flush()?;
    let mut password = String::new();
    let read = std::io::BufReader::new(&tty).read_line(&mut password);
    let restored = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &old) };
    writeln!(writer)?;
    read?;
    if restored != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(password.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(unix))]
fn read_password_from_tty() -> Result<String> {
    Err(anyhow::anyhow!(
        "hash-password needs --stdin on this platform"
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            // `guest=debug` matters: everything a guest component logs through
            // `sys::log` arrives under the `guest` target, not `thetis`, so
            // without this every guest Debug line was silently dropped and
            // anything the agent measured about itself was unobservable.
            EnvFilter::try_from_env("THETIS_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,thetis=debug,guest=debug")),
        )
        .with_target(false)
        .init();

    thetis::control::mark_start();

    // One binary, two roles. `thetis` is the gateway; the gateway spawns
    // `thetis worker --worktree <dir>` children for the conversations.
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("worker") => {
            let mut worktree = None;
            let mut session = None;
            let mut probe = false;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--worktree" => worktree = args.next().map(std::path::PathBuf::from),
                    "--session" => session = args.next(),
                    "--probe" => probe = true,
                    _ => {}
                }
            }
            if probe {
                // The gateway's pre-adoption check on a branch-built kernel:
                // does the binary start and speak the current IPC protocol?
                println!("thetis-worker-probe-ok {}", thetis::ipc::PROTOCOL_VERSION);
                return Ok(());
            }
            thetis::roles::worker::run(session, worktree).await
        }
        Some("hash-password") => {
            let password = if args.next().as_deref() == Some("--stdin") {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s.trim_end().to_string()
            } else {
                read_password_from_tty()?
            };
            println!("{}", thetis::auth::hash_password(&password)?);
            Ok(())
        }
        // A dry run of the one thing that decides whether the process can
        // start at all. Configuration is validated during boot, so a mistyped
        // key or a half-written `[[users]]` block is otherwise discovered by
        // restarting and finding Thetis gone — which is the worst moment to
        // learn it, and the moment people are in when they edit auth.
        Some("check-config") => {
            let cfg = thetis::config::Config::load()?;
            println!("configuration loads.");
            println!("  auth mode:     {}", if cfg.auth.users_mode { "users" } else { "local" });
            if cfg.auth.users_mode {
                println!("  users:         {}", cfg.auth.users.len());
                for u in &cfg.auth.users {
                    let policy = &u.policy;
                    println!(
                        "    - {} ({}) role={}{} models={}",
                        u.id,
                        u.name,
                        u.role,
                        if policy.admin { ", admin" } else { "" },
                        if policy.models_restricted {
                            format!("{} of {}", policy.models.len(), cfg.models.len())
                        } else {
                            format!("all {}", policy.models.len())
                        },
                    );
                }
                println!("  claim_unowned: {}", cfg.auth.claim_unowned);
            }
            println!("  models:        {}", cfg.models.len());
            println!("  bind:          {}", cfg.bind_addr);
            Ok(())
        }
        _ => thetis::roles::gateway::run().await,
    }
}
