use std::path::Path;

use anyhow::Result;
use tracing::info;

pub async fn run_apply(source_path: &Path, target_path: &Path, detached: bool) -> Result<()> {
    info!(?source_path, ?target_path, detached, "enzyme apply");
    println!(
        "enzyme apply: not yet implemented (source={}, target={}, detached={})",
        source_path.display(),
        target_path.display(),
        detached,
    );
    Ok(())
}
