//! Runtime paths never refer to the compilation checkout.
use std::path::PathBuf;
fn home() -> PathBuf { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(||PathBuf::from(".")) }
pub fn config_dir() -> PathBuf {
    std::env::var_os("TWIG_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(||
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(||home().join(".config")).join("twig"))
}
pub fn state_dir() -> PathBuf {
    std::env::var_os("TWIG_STATE_DIR").map(PathBuf::from).unwrap_or_else(||
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(||home().join(".local/state")).join("twig"))
}
pub fn config_path() -> PathBuf {
    std::env::var_os("TWIG_CONFIG").map(PathBuf::from).unwrap_or_else(||config_dir().join("harness.json"))
}
pub fn session_id() -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{}-{n:x}",std::process::id())
}
pub fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() < 100 && id.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'-' || b==b'_')
}
