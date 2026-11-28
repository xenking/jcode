//! Home-relative paths, shared by the config consumers that read files named in
//! `config.toml`.

use std::path::PathBuf;

/// Expand a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}
