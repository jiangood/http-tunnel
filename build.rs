use std::env;

use anyhow::Result;
use vergen::{vergen, Config, SemverKind};

// The library is built as a shared and static library for the mobile client, where
// `env!` isn't available (`VERGEN_*` are only passed to the crate that runs the
// build script). The variables that `cli.rs` reads at compile time are therefore
// declared here for the library targets.
fn emit_lib_defaults() {
    let vars = [
        "VERGEN_BUILD_SEMVER",
        "VERGEN_BUILD_TIMESTAMP",
        "VERGEN_CARGO_TARGET_TRIPLE",
        "VERGEN_CARGO_PROFILE",
        "VERGEN_CARGO_FEATURES",
    ];

    for var in vars {
        if env::var_os(var).is_none() {
            println!("cargo:rustc-env={}=unknown", var);
        }
    }
}

fn main() -> Result<()> {
    let mut config = Config::default();
    // Change the SEMVER output to the lightweight variant
    *config.git_mut().semver_kind_mut() = SemverKind::Lightweight;
    // Add a `-dirty` flag to the SEMVER output
    *config.git_mut().semver_dirty_mut() = Some("-dirty");
    // Generate the instructions
    let result = if let Err(e) = vergen(config) {
        eprintln!("error occurred while generating instructions: {:?}", e);
        let mut config = Config::default();
        *config.git_mut().enabled_mut() = false;
        vergen(config)
    } else {
        Ok(())
    };

    emit_lib_defaults();

    result
}
