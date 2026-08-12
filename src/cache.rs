use std::env;
use std::path::PathBuf;

pub fn default_cache_dir() -> Option<PathBuf> {
    if let Some(path) = env::var_os("MONORIPPLE_CACHE_DIR") {
        return Some(PathBuf::from(path));
    }

    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(path).join("monoripple"));
    }

    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/monoripple"))
}
