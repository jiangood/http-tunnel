use anyhow::Result;
use vergen::{vergen, Config};

fn main() -> Result<()> {
    // `vergen` embeds the build timestamp and the cargo environment. The `git`
    // feature is deliberately not enabled: it pulls in libgit2, whose build script
    // does not request `advapi32` on Windows with recent toolchains, which breaks
    // the build. The version then comes from `VERGEN_BUILD_SEMVER` (the package
    // version) instead of the git semver.
    let config = Config::default();
    vergen(config)
}
