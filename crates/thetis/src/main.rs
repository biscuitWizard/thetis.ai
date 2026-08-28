use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("THETIS_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,thetis=debug")),
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
                println!(
                    "thetis-worker-probe-ok {}",
                    thetis::ipc::PROTOCOL_VERSION
                );
                return Ok(());
            }
            thetis::roles::worker::run(session, worktree).await
        }
        _ => thetis::roles::gateway::run().await,
    }
}
