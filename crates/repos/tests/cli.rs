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

/// A bare origin real enough to `git clone`, quoted as a JSON string literal
/// so its path can drop straight into a `tilt-devenv.json` fixture.
fn bare_origin_url() -> (TempDir, String) {
    repos_core::gittest::isolate();
    let seed = repos_core::gittest::init_repo();
    let origin = repos_core::gittest::clone_bare(seed.path());
    let url = origin.path().to_string_lossy().into_owned();
    (origin, url)
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
fn list_json_omits_access_error_without_the_flag() {
    let root = fixture(r#"[{"name":"foo","url":"/no/such/remote","group":"g"}]"#);
    let out = repos(&root).args(["list", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        !v[0].as_object().unwrap().contains_key("accessError"),
        "accessError must be absent unless --check-access is passed"
    );
}

#[test]
fn list_check_access_reports_per_repo_reachability() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"[{{"name":"reachable","url":{},"group":"g"}},
            {{"name":"broken","url":"/no/such/remote","group":"g"}}]"#,
        serde_json::to_string(&url).unwrap(),
    ));

    let out = repos(&root)
        .args(["list", "--json", "--check-access"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let by_name = |name: &str| v.as_array().unwrap().iter().find(|i| i["name"] == name);

    assert!(
        !by_name("reachable")
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("accessError"),
        "a reachable repo has no accessError"
    );
    assert!(
        by_name("broken").unwrap()["accessError"]
            .as_str()
            .unwrap()
            .contains("/no/such/remote"),
        "broken: {:?}",
        by_name("broken")
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
fn pull_clones_a_missing_repo_before_pulling_it() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"[{{"name":"repo","url":{},"group":"g"}}]"#,
        serde_json::to_string(&url).unwrap(),
    ));

    let out = repos(&root).arg("pull").output().unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("cloned repo"), "stdout: {stdout}");
    assert!(
        repos_core::git::is_repo(&root.path().join("repo")),
        "pull should have cloned the repo before pulling it"
    );
}

#[test]
fn checkout_clones_a_missing_repo_before_switching_it() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"[{{"name":"repo","url":{},"group":"g"}}]"#,
        serde_json::to_string(&url).unwrap(),
    ));

    let out = repos(&root).args(["checkout", "default"]).output().unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("cloned repo"), "stdout: {stdout}");
    assert!(
        repos_core::git::is_repo(&root.path().join("repo")),
        "checkout should have cloned the repo before switching it"
    );
}

#[test]
fn checkout_dry_run_does_not_clone_a_missing_repo() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"[{{"name":"repo","url":{},"group":"g"}}]"#,
        serde_json::to_string(&url).unwrap(),
    ));

    repos(&root)
        .args(["checkout", "default", "--dry-run"])
        .assert()
        .success();

    assert!(
        !root.path().join("repo").exists(),
        "--dry-run must not clone or change anything on disk"
    );
}

#[test]
fn pull_stays_quiet_about_cloning_when_everything_is_already_present() {
    let root = fixture(r#"[{"name":"repo","url":"u","group":"g"}]"#);
    let repo_path = root.path().join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    repos_core::gittest::isolate();
    repos_core::gittest::git(&repo_path, &["init", "-q", "-b", "main"]);
    repos_core::gittest::commit(&repo_path, "README.md", "hi\n", "init");

    let out = repos(&root).arg("pull").output().unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("already present"),
        "nothing needed cloning, so the clone tally shouldn't print at all: {stdout}"
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
fn clone_json_reports_cloned_and_already_present() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"[{{"name":"absent","url":{},"group":"g"}},
            {{"name":"already","url":"u","group":"g"}}]"#,
        serde_json::to_string(&url).unwrap(),
    ));
    std::fs::create_dir_all(root.path().join("already").join(".git")).unwrap();

    let out = repos(&root).args(["clone", "--json"]).output().unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let by_name = |name: &str| v.as_array().unwrap().iter().find(|i| i["name"] == name);

    assert_eq!(by_name("absent").unwrap()["outcome"], "cloned");
    assert_eq!(by_name("already").unwrap()["outcome"], "already-present");
    assert!(
        repos_core::git::is_repo(&root.path().join("absent")),
        "clone should have left a working tree on disk"
    );
}

#[test]
fn clone_defaults_to_the_active_profile_selection() {
    let (_origin_web, url_web) = bare_origin_url();
    let (_origin_auth, url_auth) = bare_origin_url();
    let root = fixture(&format!(
        r#"{{"repos":[{{"name":"web","url":{},"group":"frontend"}},
                       {{"name":"auth","url":{},"group":"backend"}}],
            "profiles":{{"frontend":["frontend"]}}}}"#,
        serde_json::to_string(&url_web).unwrap(),
        serde_json::to_string(&url_auth).unwrap(),
    ));
    repos(&root)
        .args(["profile", "set", "frontend"])
        .assert()
        .success();

    repos(&root).arg("clone").assert().success();

    assert!(repos_core::git::is_repo(&root.path().join("web")));
    assert!(
        !repos_core::git::is_repo(&root.path().join("auth")),
        "auth isn't in the active profile, so a plain `clone` must leave it alone"
    );
}

#[test]
fn clone_only_overrides_the_active_profile_restriction() {
    let (_origin_web, url_web) = bare_origin_url();
    let (_origin_auth, url_auth) = bare_origin_url();
    let root = fixture(&format!(
        r#"{{"repos":[{{"name":"web","url":{},"group":"frontend"}},
                       {{"name":"auth","url":{},"group":"backend"}}],
            "profiles":{{"frontend":["frontend"]}}}}"#,
        serde_json::to_string(&url_web).unwrap(),
        serde_json::to_string(&url_auth).unwrap(),
    ));
    repos(&root)
        .args(["profile", "set", "frontend"])
        .assert()
        .success();

    repos(&root)
        .args(["clone", "--only=auth"])
        .assert()
        .success();

    assert!(
        repos_core::git::is_repo(&root.path().join("auth")),
        "--only names auth exactly, so it must reach outside the active profile"
    );
}

#[test]
fn clone_clones_nothing_when_profiles_exist_and_none_is_active() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"{{"repos":[{{"name":"web","url":{},"group":"frontend"}}],
            "profiles":{{"frontend":["frontend"]}}}}"#,
        serde_json::to_string(&url).unwrap(),
    ));

    repos(&root)
        .arg("clone")
        .assert()
        .success()
        .stderr(predicates::str::contains("no active profile"));

    assert!(
        !root.path().join("web").exists(),
        "a bare `repos clone` before ever picking a profile must not clone everything"
    );
}

#[test]
fn pull_and_checkout_do_nothing_when_profiles_exist_and_none_is_active() {
    let (_origin, url) = bare_origin_url();
    let root = fixture(&format!(
        r#"{{"repos":[{{"name":"web","url":{},"group":"frontend"}}],
            "profiles":{{"frontend":["frontend"]}}}}"#,
        serde_json::to_string(&url).unwrap(),
    ));

    repos(&root)
        .arg("pull")
        .assert()
        .success()
        .stderr(predicates::str::contains("no active profile"));
    assert!(
        !root.path().join("web").exists(),
        "pull must not reach every repo just because no profile is active"
    );

    repos(&root)
        .args(["checkout", "default"])
        .assert()
        .success()
        .stderr(predicates::str::contains("no active profile"));
    assert!(
        !root.path().join("web").exists(),
        "checkout must not reach every repo just because no profile is active"
    );
}

#[test]
fn pull_and_checkout_skip_an_already_cloned_repo_too_when_unscoped() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    let repo_path = root.path().join("web");
    std::fs::create_dir_all(&repo_path).unwrap();
    repos_core::gittest::isolate();
    repos_core::gittest::git(&repo_path, &["init", "-q", "-b", "main"]);
    repos_core::gittest::commit(&repo_path, "README.md", "hi\n", "init");

    let out = repos(&root).arg("pull").output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout).unwrap().is_empty(),
        "pull must act on nothing, not just skip cloning it"
    );

    let out = repos(&root).args(["checkout", "default"]).output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout).unwrap().is_empty(),
        "checkout must act on nothing, not just skip cloning it"
    );
}

#[test]
fn status_shows_nothing_when_profiles_exist_and_none_is_active() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );

    let out = repos(&root).args(["status", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, serde_json::json!([]), "nothing is in scope to report on");

    repos(&root)
        .arg("status")
        .assert()
        .success()
        .stderr(predicates::str::contains("no active profile"));
}

#[test]
fn status_covers_the_whole_registry_when_no_profiles_are_defined() {
    let root = fixture(r#"[{"name":"foo","url":"u","group":"g"}]"#);
    let out = repos(&root).args(["status", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v.as_array().unwrap().len(),
        1,
        "nothing to scope to, so status still covers everything"
    );
}

#[test]
fn clone_group_outside_active_profile_errors() {
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    repos(&root)
        .args(["profile", "set", "frontend"])
        .assert()
        .success();

    repos(&root)
        .args(["clone", "--group=backend"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth"));
}

#[test]
fn list_shows_repos_outside_the_active_profile_too() {
    // Regression: `repos list` must stay a full inventory view. The Tiltfile's
    // `repos_load()` is built on `repos list --json` and needs every repo's
    // metadata (even present=false ones) to do its own profile-based resource
    // gating — if `list` pre-filtered, an out-of-scope repo would vanish from
    // the dict entirely instead of just showing as not cloned.
    let root = fixture(
        r#"{"repos":[{"name":"web","url":"u","group":"frontend"},{"name":"auth","url":"u","group":"backend"}],
            "profiles":{"frontend":["frontend"]}}"#,
    );
    repos(&root)
        .args(["profile", "set", "frontend"])
        .assert()
        .success();

    let out = repos(&root).args(["list", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["web", "auth"],
        "list must show every repo regardless of the active profile"
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
fn status_watch_rejects_json() {
    let root = fixture(r#"[{"name":"foo","url":"u","group":"g"}]"#);
    repos(&root)
        .args(["status", "--watch", "--json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--watch"));
}

#[test]
fn status_watch_reprints_only_when_local_state_changes() {
    let root = fixture(r#"[{"name":"repo","url":"u","group":"g"}]"#);
    let repo_path = root.path().join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    repos_core::gittest::isolate();
    repos_core::gittest::git(&repo_path, &["init", "-q", "-b", "main"]);
    repos_core::gittest::commit(&repo_path, "README.md", "hi\n", "init");

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("repos"))
        .env("REPOS_ROOT", root.path())
        .env("XDG_STATE_HOME", root.path().join(".state"))
        .env("NO_COLOR", "1")
        .args(["status", "--watch", "--interval", "50ms"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(200));
    repos_core::gittest::write(&repo_path, "dirty.txt", "x");
    std::thread::sleep(std::time::Duration::from_millis(300));
    child.kill().unwrap();
    let stdout = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();

    assert_eq!(
        stdout.matches("clean").count(),
        1,
        "one printout before the mutation:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("dirty").count(),
        1,
        "one printout after the mutation, not one per interval tick:\n{stdout}"
    );
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
