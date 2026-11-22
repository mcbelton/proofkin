//! Its own test binary because it changes this process's environment.

use std::ffi::OsString;
use std::path::Path;

use proofkin::llm::Bridge;
use proofkin::{Access, LlmTask, Provider};

#[tokio::test]
async fn bridge_gets_only_the_profile_credentials() {
    // SAFETY: this binary has a single test, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "machine-wide-key");
        std::env::set_var("GH_TOKEN", "machine-wide-token");
        std::env::set_var("CODEX_HOME", "C:/machine-wide/.codex");
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_bridge.mjs");
    let env: Vec<(String, OsString)> = vec![("CODEX_HOME".into(), "profile-codex-home".into())];
    let bridge = Bridge::spawn_node(&script, env).unwrap();

    let task = LlmTask::new(Provider::Codex, "env:ANTHROPIC_API_KEY,GH_TOKEN,CODEX_HOME");
    let out = bridge
        .run(&task, Path::new("."), Access::ReadOnly)
        .await
        .unwrap();
    assert_eq!(
        out.output,
        "ANTHROPIC_API_KEY=<unset>;GH_TOKEN=<unset>;CODEX_HOME=profile-codex-home"
    );
}
