pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const GIT_REVISION: &str = match option_env!("BUILD_REVISION") {
    Some(value) => value,
    None => "unknown",
};
pub(crate) const TARGET: &str = std::env::consts::ARCH;
pub(crate) const DISPLAY_NAME: &str = "Twitch Channel Points Miner";
pub(crate) const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");

pub(crate) fn version_banner() -> String {
    format!("{VERSION} ({GIT_REVISION}; {TARGET})")
}
