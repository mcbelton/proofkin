use std::path::Path;

use proofkin::llm::Bridge;
use proofkin::{Access, LlmTask, Provider};

fn fake_bridge() -> Bridge {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_bridge.mjs");
    Bridge::spawn_node(&script, Vec::new()).expect("node must be installed to run bridge tests")
}

#[tokio::test]
async fn sends_task_fields_and_returns_text() {
    let bridge = fake_bridge();
    let task = LlmTask::new(Provider::Codex, "fix the build");
    let out = bridge
        .run(&task, Path::new("."), Access::WorkspaceWrite)
        .await
        .unwrap();
    assert_eq!(out.output, "codex|workspace-write|true|fix the build");
    // The reply names the model that answered: the one asked for, else the SDK's.
    assert_eq!(out.model.as_deref(), Some("codex-default-model"));
    let mut chosen = LlmTask::new(Provider::Claude, "review it");
    chosen.model = Some("claude-opus-5.5".into());
    let out = bridge
        .run(&chosen, Path::new("."), Access::ReadOnly)
        .await
        .unwrap();
    assert_eq!(out.model.as_deref(), Some("claude-opus-5.5"));
    // An older bridge's reply, without a model, still reads.
    let older = bridge
        .run(
            &LlmTask::new(Provider::Claude, "cancelled?"),
            Path::new("."),
            Access::ReadOnly,
        )
        .await
        .unwrap();
    assert_eq!(older.model, None);
}

#[tokio::test]
async fn multiplexes_concurrent_requests() {
    let bridge = fake_bridge();
    let slow = LlmTask::new(Provider::Claude, "slow");
    let fast = LlmTask::new(Provider::Codex, "fast");
    let (slow, fast) = tokio::join!(
        bridge.run(&slow, Path::new("."), Access::ReadOnly),
        bridge.run(&fast, Path::new("."), Access::ReadOnly),
    );
    assert_eq!(slow.unwrap().output, "claude|read-only|true|slow");
    assert_eq!(fast.unwrap().output, "codex|read-only|true|fast");
}

#[tokio::test]
async fn surfaces_provider_errors_and_bridge_exit() {
    let bridge = fake_bridge();
    let err = bridge
        .run(
            &LlmTask::new(Provider::Claude, "fail"),
            Path::new("."),
            Access::ReadOnly,
        )
        .await;
    assert_eq!(err.unwrap_err().to_string(), "boom");

    let exit = bridge
        .run(
            &LlmTask::new(Provider::Claude, "exit"),
            Path::new("."),
            Access::ReadOnly,
        )
        .await;
    assert_eq!(exit.unwrap_err().to_string(), "bridge process exited");
}

#[tokio::test]
async fn cancelling_a_request_tells_the_bridge_to_abort() {
    let bridge = fake_bridge();
    let hang = LlmTask::new(Provider::Claude, "hang");
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        bridge.run(&hang, Path::new("."), Access::ReadOnly),
    )
    .await;
    assert!(dropped.is_err(), "the hanging request was dropped");
    // The cancel line is written in the background; the bridge sees requests in order.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let seen = bridge
        .run(
            &LlmTask::new(Provider::Claude, "cancelled?"),
            Path::new("."),
            Access::ReadOnly,
        )
        .await
        .unwrap();
    assert_eq!(seen.output, "1", "bridge received cancel for request 1");
}

#[test]
fn claude_tool_policy_confines_paths_and_tools() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("node")
        .arg(root.join("tests/fixtures/bridge_policy.mjs"))
        .env("PROOFKIN_BRIDGE_IMPORT_ONLY", "1")
        .output()
        .expect("node must be installed to run bridge tests");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
