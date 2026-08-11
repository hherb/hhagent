//! Binary entry point: env-resolved allowlist, then the prelude's lockdown +
//! serve loop (Landlock + seccomp + rlimit before any I/O).

use kastellan_worker_prelude::serve_stdio;
use kastellan_worker_shell_exec::handler::ShellExecHandler;

fn main() -> anyhow::Result<()> {
    let mut handler = ShellExecHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
