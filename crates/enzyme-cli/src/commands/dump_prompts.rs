use anyhow::Result;
use std::path::{Path, PathBuf};

pub async fn run_dump_prompts(
    _vault_path: &Path,
    _output: &PathBuf,
    _guide: Option<String>,
    _min_top_catalysts: Option<usize>,
) -> Result<()> {
    todo!("dump_prompts not yet implemented")
}
