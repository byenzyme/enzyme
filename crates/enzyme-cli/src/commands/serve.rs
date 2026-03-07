use std::path::Path;

use anyhow::Result;

pub async fn run_serve(vault_path: &Path, port: u16) -> Result<()> {
    enzyme_api::serve(vault_path.to_path_buf(), port).await
}
