//! End-to-end tests that exec the `repos` binary and lock the `--json` output
//! contract the Tiltfile depends on (field shape + exact formatting).

use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

/// A REPOS_ROOT with a `tilt-devenv.json` fixture; `REPOS_ROOT` short-circuits the
/// upward search so the test never picks up the real registry.
fn fixture(repos_json: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("tilt-devenv.json"), repos_json).unwrap();
    dir
}

fn repos(root: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("repos").unwrap();
    cmd.env("REPOS_ROOT", root.path())
        .env("XDG_STATE_HOME", root.path().join(".state"))
        .env("NO_COLOR", "1");
    cmd
}

#[test]
fn list_json_has_the_expected_shape_and_formatting() {
    let root = fixture(r#"[{"name":"foo","url":"u","group":"g"}]"#);
    let out = repos(&root).args(["list", "--json"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();

    // 2-space indent, name-first key order, trailing newline — the byte contract.
    assert!(
        stdout.starts_with("[\n  {\n    \"name\": \"foo\","),
        "got: {stdout}"
    );
    assert!(
        stdout.ends_with("\n]\n"),
        "must end with a trailing newline"
    );

    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let item = &v[0];
    assert_eq!(item["name"], "foo");
    assert_eq!(item["url"], "u");
    assert_eq!(item["group"], "g");
    assert_eq!(item["present"], false); // nothing cloned on disk
    assert!(
        item["path"].as_str().unwrap().ends_with("/foo"),
        "sibling path resolution"
    );
}

#[test]
fn status_json_omits_empty_fields_for_an_uncloned_repo() {
    let root = fixture(r#"[{"name":"foo","url":"u","group":"g"}]"#);
    let out = repos(&root).args(["status", "--json"]).output().unwrap();
    assert!(out.status.success());

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let item = v[0].as_object().unwrap();

    // Always present.
    assert_eq!(item["name"], "foo");
    assert_eq!(item["present"], false);
    assert_eq!(item["ahead"], 0);
    assert_eq!(item["dirty"], false);
    assert_eq!(item["onDefault"], false);
    // Empty strings are skipped (omitempty), not serialized as "".
    for absent in [
        "branch",
        "upstream",
        "defaultBranch",
        "detached",
        "error",
        "fetchError",
    ] {
        assert!(
            !item.contains_key(absent),
            "{absent} should be omitted when empty"
        );
    }
}

#[test]
fn profiles_json_has_the_expected_shape() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"],"backend":["auth"]}}"#,
    );
    let out = repos(&root).args(["profiles", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["frontend"], serde_json::json!(["frontend"]));
    assert_eq!(v["backend"], serde_json::json!(["auth"]));
}

#[test]
fn status_profile_flag_restricts_to_the_profiles_repos() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    let out = repos(&root)
        .args(["status", "--profile=frontend", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["web"], "only the frontend profile's repo");
}

#[test]
fn checkout_profile_flag_restricts_to_the_profiles_repos() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    let out = repos(&root)
        .args(["checkout", "default", "--profile=frontend", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("web"), "stdout: {stdout}");
    assert!(
        !stdout.contains("auth"),
        "auth excluded by --profile; stdout: {stdout}"
    );
}

#[test]
fn checkout_only_and_profile_flags_union() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"},{"name":"billing","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    let out = repos(&root)
        .args([
            "checkout",
            "default",
            "--only=auth",
            "--profile=frontend",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("web"), "from --profile; stdout: {stdout}");
    assert!(stdout.contains("auth"), "from --only; stdout: {stdout}");
    assert!(
        !stdout.contains("billing"),
        "matches neither; stdout: {stdout}"
    );
}

#[test]
fn pull_profile_flag_restricts_to_the_profiles_repos() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    let out = repos(&root)
        .args(["pull", "--profile=frontend"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("web"), "stdout: {stdout}");
    assert!(
        !stdout.contains("auth"),
        "auth excluded by --profile; stdout: {stdout}"
    );
}

#[test]
fn profile_active_persists_across_invocations_and_clears() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"}],
            "profiles":{"frontend":["web"],"backend":["auth"]}}"#,
    );

    let out = repos(&root)
        .args(["profile", "active", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!([]),
        "nothing set yet"
    );

    repos(&root)
        .args(["profile", "set", "frontend,backend"])
        .assert()
        .success();
    let out = repos(&root)
        .args(["profile", "active", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!(["frontend", "backend"]),
        "persisted across the two separate `repos` invocations"
    );

    repos(&root).args(["profile", "set"]).assert().success();
    let out = repos(&root)
        .args(["profile", "active", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!([]),
        "`profile set` with nothing clears back to every profile enabled"
    );
}

#[test]
fn missing_registry_fails_with_a_clear_error() {
    let empty = TempDir::new().unwrap();
    repos(&empty)
        .arg("list")
        .assert()
        .failure()
        .stderr(predicates::str::contains("tilt-devenv.json"));
}

#[test]
fn dynamic_completion_emits_a_shell_script() {
    // The flake's postInstall relies on `COMPLETE=<shell> repos` printing an
    // integration script and exiting 0.
    let bin = assert_cmd::cargo::cargo_bin("repos");
    for shell in ["bash", "zsh", "fish"] {
        let out = StdCommand::new(&bin)
            .env("COMPLETE", shell)
            .output()
            .unwrap();
        assert!(out.status.success(), "COMPLETE={shell} should exit 0");
        assert!(
            !out.stdout.is_empty(),
            "COMPLETE={shell} should print a script"
        );
    }
}
