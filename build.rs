fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // Clap builds the whole command tree before parsing a command. In debug builds
        // its generated code needs more than the default 1 MiB Windows stack reserve.
        println!("cargo::rustc-link-arg-bin=proofkin=/STACK:2097152");
    }
}
