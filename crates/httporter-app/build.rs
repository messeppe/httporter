fn main() {
    let mut config = slint_build::CompilerConfiguration::new().with_style("cupertino".into());
    // The headless tests look elements up by accessible role and label, which needs the element
    // debug info. Only debug builds (what `cargo test` uses) carry it; release builds stay lean.
    if std::env::var("PROFILE").is_ok_and(|profile| profile == "debug") {
        config = config.with_debug_info(true);
    }
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
}
