use std::env;
use std::path::PathBuf;

/// Where Pulse keeps its local data. `PULSE_HOME` overrides it.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = env::var_os("PULSE_HOME") {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(env::temp_dir)
        .join("pulse")
}

pub fn claude_dir() -> Option<PathBuf> {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

pub fn codex_dir() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}
