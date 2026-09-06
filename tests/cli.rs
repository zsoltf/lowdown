use std::process::Command;

fn lowdown() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lowdown"));
    command.env("LOWDOWN_SUMMARY_PROVIDER", "fallback");
    command
}

#[test]
fn installed_surface_reports_version() {
    let output = lowdown().arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("lowdown {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_intervals_fail_without_panicking() {
    for value in ["inf", "NaN", "-1", "0", "999999999999"] {
        let output = lowdown()
            .args(["watch", &format!("--poll-seconds={value}")])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn explicit_fixture_digest_and_json_bootstrap_work_offline() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_rollout.jsonl"
    );
    let cache = tempfile::tempdir().unwrap();
    let digest = lowdown()
        .env("LOWDOWN_CACHE_DIR", cache.path())
        .args(["digest", "--session", fixture, "--pretty"])
        .output()
        .unwrap();
    assert!(
        digest.status.success(),
        "{}",
        String::from_utf8_lossy(&digest.stderr)
    );
    assert!(String::from_utf8_lossy(&digest.stdout).contains("Latest answer"));
    let bootstrap = lowdown()
        .args(["recent-updates", "--session", fixture, "--format", "json"])
        .output()
        .unwrap();
    assert!(bootstrap.status.success());
    let value: serde_json::Value = serde_json::from_slice(&bootstrap.stdout).unwrap();
    assert_eq!(value["updates"].as_array().unwrap().len(), 2);
}

#[test]
fn redirected_watch_explains_the_text_alternative() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_rollout.jsonl"
    );
    let output = lowdown()
        .args(["watch", "--session", fixture])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("lowdown digest --pretty"));
}
