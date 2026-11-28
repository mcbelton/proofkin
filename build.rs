fn main() {
    println!("cargo::rerun-if-env-changed=PROOFKIN_RELEASE_REPO");
    if let Ok(repo) = std::env::var("PROOFKIN_RELEASE_REPO") {
        println!("cargo::rustc-env=PROOFKIN_RELEASE_REPO={repo}");
    }
    let target = std::env::var("TARGET").expect("Cargo sets TARGET");
    println!("cargo::rustc-env=PROOFKIN_TARGET={target}");

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
