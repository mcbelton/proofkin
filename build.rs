fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // In debug builds, clap's generated code for the whole command tree can
    // overflow the default 1 MiB stack before it dispatches any command.
    let stack = match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => "/STACK:4194304",
        Ok("gnu") => "-Wl,--stack,4194304",
        _ => return,
    };
    println!("cargo:rustc-link-arg-bin=proofkin={stack}");
}
