use std::sync::Arc;

use anyhow::Result;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::embedding::daemon_protocol::{PID_FILENAME, SOCKET_FILENAME};
use enzyme_core::embedding::daemon_server::DaemonServer;

/// Entry point for `enzyme __daemon` (hidden subcommand).
pub async fn run_daemon() -> Result<()> {
    let enzyme_home = EnzymePaths::enzyme_home()?;

    let socket_path = enzyme_home.join(SOCKET_FILENAME);
    let pid_path = enzyme_home.join(PID_FILENAME);

    let server = Arc::new(DaemonServer::new(&enzyme_home)?);
    server.run(&socket_path, &pid_path).await
}
