use std::path::{Path, PathBuf};

pub(crate) fn resolve_shared_model_dir(
    out_dir: &Path,
    model_cache: Option<&Path>,
    cache_dir_name: &str,
) -> Result<PathBuf, String> {
    if let Some(model_cache) = model_cache.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(model_cache.to_path_buf());
    }

    let target_dir = out_dir
        .ancestors()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("target"))
        })
        // OUT_DIR normally has the form
        // <target-dir>/<profile>/build/<package>/out. The target directory is
        // allowed to have any name, so retain that layout as a fallback.
        .or_else(|| out_dir.ancestors().nth(4))
        .ok_or_else(|| {
            format!(
                "could not resolve Cargo target directory from OUT_DIR {}",
                out_dir.display()
            )
        })?;

    Ok(target_dir.join(cache_dir_name))
}
