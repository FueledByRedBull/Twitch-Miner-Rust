pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const GIT_REVISION: &str = env!("TM_GIT_REVISION");
pub(crate) const TARGET: &str = env!("TM_BUILD_TARGET");
pub(crate) const DISPLAY_NAME: &str = "Twitch Channel Points Miner";
pub(crate) const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");
pub(crate) const VERSION_BANNER: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TM_GIT_REVISION"),
    "; ",
    env!("TM_BUILD_TARGET"),
    "; built ",
    env!("TM_BUILD_TIME"),
    ")"
);
