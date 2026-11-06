// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::path::PathBuf;

fn main() {
    let mut config = slint_build::CompilerConfiguration::new().with_style("cupertino".into());
    // The headless tests look elements up by accessible role and label, which needs the element
    // debug info. Only debug builds (what `cargo test` uses) carry it; release builds stay lean.
    if env::var("PROFILE").is_ok_and(|profile| profile == "debug") {
        config = config.with_debug_info(true);
    }
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
    embed_resources();
}

/// The exe's icon, version information and manifest (`assets/icon/httporter.rc`), linked into the
/// `httporter` binary only. The version comes from `CARGO_PKG_VERSION`, through a `version.h`
/// written into `OUT_DIR` (which embed-resource adds to rc.exe's search path).
fn embed_resources() {
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os != "windows") {
        return;
    }
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let icon_dir = manifest_dir.join("../../assets/icon");
    for file in ["httporter.rc", "httporter.ico", "httporter.exe.manifest"] {
        println!("cargo:rerun-if-changed={}", icon_dir.join(file).display());
    }

    let version = env::var("CARGO_PKG_VERSION").expect("set by cargo");
    let mut numbers = version
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u16>().unwrap_or(0));
    let mut next = || numbers.next().unwrap_or(0);
    let (major, minor, patch) = (next(), next(), next());
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("set by cargo"));
    std::fs::write(
        out_dir.join("version.h"),
        format!(
            "#define VER_MAJOR {major}\n#define VER_MINOR {minor}\n#define VER_PATCH {patch}\n\
             #define VER_STRING \"{version}\"\n"
        ),
    )
    .expect("version.h can be written");

    embed_resource::compile_for(
        icon_dir.join("httporter.rc"),
        ["httporter"],
        embed_resource::ParamsIncludeDirs([icon_dir.as_os_str()]),
    )
    .manifest_required()
    .expect("rc.exe compiles assets/icon/httporter.rc");
}
