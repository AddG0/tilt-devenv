//! The low-level Tilt client (the seam): build UIButtons, apply/delete them via
//! the `tilt` CLI, and stream button clicks from Tilt's apiserver. Knows nothing
//! about *which* buttons a caller shows — that's the caller's concern.
//!
//! Buttons are created *unowned* (no Tiltfile ownerReference) so the Tiltfile
//! controller won't reconcile them away. Clicks are delivered by [`watch_clicks`]
//! and handled in-process by the caller, so an action runs in the watching
//! process — no separate command invocation.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// A Tilt UIButton, built fluently. Serialized straight into a manifest — JSON
/// is valid YAML, so it feeds `tilt apply -f -` directly.
#[derive(Serialize)]
pub struct UiButton {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: ObjectMeta,
    spec: UiButtonSpec,
}

impl UiButton {
    /// A button with the given object name and label; add location/icon/inputs
    /// with the builder methods.
    pub fn new(name: String, text: String) -> UiButton {
        UiButton {
            api_version: "tilt.dev/v1alpha1".to_string(),
            kind: "UIButton".to_string(),
            metadata: ObjectMeta { name },
            spec: UiButtonSpec {
                text,
                icon_name: String::new(),
                location: Location {
                    component_id: String::new(),
                    component_type: String::new(),
                },
                inputs: Vec::new(),
                disabled: false,
            },
        }
    }

    pub fn icon(mut self, name: &str) -> UiButton {
        self.spec.icon_name = name.to_string();
        self
    }

    /// Place the button: `component_type` is e.g. "Resource" or "Global", and
    /// `component_id` the resource name or "nav".
    pub fn at(mut self, component_id: &str, component_type: &str) -> UiButton {
        self.spec.location = Location {
            component_id: component_id.to_string(),
            component_type: component_type.to_string(),
        };
        self
    }

    /// Add a single free-text input the click carries back under `name`.
    pub fn text_input(mut self, name: &str, label: &str, placeholder: &str) -> UiButton {
        self.spec.inputs = vec![UiInput {
            name: name.to_string(),
            label: label.to_string(),
            text: Some(UiText {
                placeholder: placeholder.to_string(),
            }),
            choice: None,
        }];
        self
    }

    /// Add a single dropdown input the click carries back under `name` (the
    /// chosen string arrives as that input's value).
    pub fn choice_input(mut self, name: &str, label: &str, choices: Vec<String>) -> UiButton {
        self.spec.inputs = vec![UiInput {
            name: name.to_string(),
            label: label.to_string(),
            text: None,
            choice: Some(UiChoice { choices }),
        }];
        self
    }

    pub fn disabled(mut self, disabled: bool) -> UiButton {
        self.spec.disabled = disabled;
        self
    }
}

/// A Kubernetes-style object's metadata — serialized into manifests and parsed
/// back from `tilt get -o json`.
#[derive(Serialize, Deserialize, Default)]
struct ObjectMeta {
    #[serde(default)]
    name: String,
}

#[derive(Serialize)]
struct UiButtonSpec {
    text: String,
    #[serde(rename = "iconName", skip_serializing_if = "String::is_empty")]
    icon_name: String,
    location: Location,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<UiInput>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    disabled: bool,
}

#[derive(Serialize)]
struct Location {
    #[serde(rename = "componentID")]
    component_id: String,
    #[serde(rename = "componentType")]
    component_type: String,
}

#[derive(Serialize)]
struct UiInput {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<UiText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    choice: Option<UiChoice>,
}

#[derive(Serialize)]
struct UiText {
    #[serde(skip_serializing_if = "String::is_empty")]
    placeholder: String,
}

#[derive(Serialize)]
struct UiChoice {
    choices: Vec<String>,
}

/// Fails with `<what>: <status>: <stderr>` when a `tilt` invocation exited
/// non-zero, so every callsite reports failures the same way.
fn check(out: &std::process::Output, what: &str) -> Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "{what}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Applies a button to Tilt's apiserver (creates or updates it).
pub fn apply(button: &UiButton) -> Result<()> {
    let doc = serde_json::to_vec(button)?;
    let mut child = Command::new("tilt")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning `tilt apply`")?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(&doc)?;
    check(&child.wait_with_output()?, "tilt apply")
}

/// A Tilt UIResource: its name and whether it carries any Tilt label. Unlabeled
/// resources are Tilt's setup/meta group (gradle-wrappers, pnpm-installs,
/// `(Tiltfile)`) rather than running services.
pub struct Resource {
    pub name: String,
    pub labeled: bool,
}

/// Every UIResource via `tilt get uiresource -o json`. Erroring here doubles as
/// a "is Tilt up?" check.
pub fn uiresources() -> Result<Vec<Resource>> {
    let out = Command::new("tilt")
        .args(["get", "uiresource", "-o", "json"])
        .output()
        .context("running `tilt get uiresource`")?;
    check(&out, "tilt get uiresource")?;
    Ok(resources_from_json(&out.stdout))
}

/// Parses a `tilt get uiresource -o json` list into name + has-labels. Malformed
/// output yields an empty list rather than an error.
fn resources_from_json(json: &[u8]) -> Vec<Resource> {
    #[derive(Deserialize, Default)]
    struct List {
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(Deserialize, Default)]
    struct Item {
        #[serde(default)]
        metadata: Meta,
    }
    #[derive(Deserialize, Default)]
    struct Meta {
        #[serde(default)]
        name: String,
        #[serde(default)]
        labels: HashMap<String, String>,
    }
    serde_json::from_slice::<List>(json)
        .unwrap_or_default()
        .items
        .into_iter()
        .filter(|i| !i.metadata.name.is_empty())
        .map(|i| Resource {
            name: i.metadata.name,
            labeled: !i.metadata.labels.is_empty(),
        })
        .collect()
}

/// Deletes a UIButton by name. A missing button is not an error.
pub fn delete_button(name: &str) -> Result<()> {
    tracing::debug!(name, "tilt delete uibutton");
    let out = Command::new("tilt")
        .args(["delete", "uibutton", name, "--ignore-not-found"])
        .output()
        .with_context(|| format!("spawning `tilt delete uibutton {name}`"))?;
    check(&out, &format!("tilt delete uibutton {name}"))
}

/// Forces Tilt to (re)run a resource's update command, regardless of its
/// trigger mode. Used to refresh the git-status resource on a git change.
pub fn trigger(resource: &str) -> Result<()> {
    tracing::debug!(resource, "tilt trigger");
    let out = Command::new("tilt")
        .args(["trigger", resource])
        .output()
        .with_context(|| format!("spawning `tilt trigger {resource}`"))?;
    check(&out, &format!("tilt trigger {resource}"))
}

/// A button press: the button's object name and its input values.
#[derive(Debug, Clone)]
pub struct Click {
    pub button: String,
    pub inputs: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
struct WatchButton {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    metadata: ObjectMeta,
    #[serde(default)]
    status: WatchStatus,
}

#[derive(Deserialize, Default)]
struct WatchStatus {
    // Tilt emits `null` for a never-clicked button, and serde won't put `null`
    // into a `String` — so this must be `Option`, or the whole object fails to
    // parse and the click stream dies on the first object.
    #[serde(rename = "lastClickedAt", default)]
    last_clicked_at: Option<String>,
    #[serde(default)]
    inputs: Vec<WatchInput>,
}

#[derive(Deserialize)]
struct WatchInput {
    name: String,
    #[serde(default)]
    text: Option<WatchValue>,
    #[serde(default)]
    choice: Option<WatchValue>,
}

#[derive(Deserialize)]
struct WatchValue {
    value: String,
}

#[derive(Deserialize, Default)]
struct WatchList {
    #[serde(default)]
    items: Vec<WatchButton>,
}

fn inputs_of(b: &WatchButton) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for input in &b.status.inputs {
        if let Some(c) = &input.choice {
            m.insert(input.name.clone(), c.value.clone());
        } else if let Some(t) = &input.text {
            m.insert(input.name.clone(), t.value.clone());
        }
    }
    m
}

/// Keeps the `tilt get --watch-only` child alive; killing it on drop ends the
/// click stream (used at daemon shutdown).
pub struct ClickWatcher {
    child: std::process::Child,
}

impl Drop for ClickWatcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Streams UIButton clicks from Tilt's apiserver. A click is reported only when
/// a button's `lastClickedAt` advances past what it was when watching began, so
/// re-applying a button's spec (or a click that predates the daemon) isn't
/// mistaken for a fresh press. The returned [`ClickWatcher`] must be kept alive;
/// dropping it stops the stream.
pub fn watch_clicks() -> Result<(tokio::sync::mpsc::UnboundedReceiver<Click>, ClickWatcher)> {
    let mut last_seen: HashMap<String, String> = HashMap::new();
    if let Ok(out) = Command::new("tilt")
        .args(["get", "uibutton", "-o", "json"])
        .output()
        && out.status.success()
        && let Ok(list) = serde_json::from_slice::<WatchList>(&out.stdout)
    {
        for b in list.items {
            last_seen.insert(
                b.metadata.name,
                b.status.last_clicked_at.unwrap_or_default(),
            );
        }
    }

    let mut child = Command::new("tilt")
        .args(["get", "uibutton", "-o", "json", "--watch-only"])
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning `tilt get --watch-only`")?;
    let stdout = child.stdout.take().expect("stdout was piped");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stream = serde_json::Deserializer::from_reader(stdout).into_iter::<WatchButton>();
        for item in stream {
            let Ok(b) = item else {
                return;
            };
            if b.kind != "UIButton" {
                continue;
            }
            let at = b.status.last_clicked_at.clone().unwrap_or_default();
            if at.is_empty() || last_seen.get(&b.metadata.name) == Some(&at) {
                continue;
            }
            last_seen.insert(b.metadata.name.clone(), at);
            let click = Click {
                button: b.metadata.name.clone(),
                inputs: inputs_of(&b),
            };
            if tx.send(click).is_err() {
                return;
            }
        }
    });

    Ok((rx, ClickWatcher { child }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_of_reads_choice_and_text() {
        let raw = r#"{"status":{"inputs":[
            {"name":"branch","choice":{"value":"feat/login"}},
            {"name":"note","text":{"value":"hi"}}
        ]}}"#;
        let b: WatchButton = serde_json::from_str(raw).unwrap();
        let got = inputs_of(&b);
        assert_eq!(got["branch"], "feat/login");
        assert_eq!(got["note"], "hi");
    }

    #[test]
    fn watch_button_tolerates_null_last_clicked_at() {
        // Tilt streams `lastClickedAt: null` for never-clicked buttons; if this
        // fails to parse, the whole click stream dies on the first object.
        let raw = r#"{"kind":"UIButton","metadata":{"name":"b"},"status":{"lastClickedAt":null}}"#;
        let b: WatchButton = serde_json::from_str(raw).unwrap();
        assert_eq!(b.status.last_clicked_at, None);
    }

    #[test]
    fn resources_from_json_reads_names_labels_and_drops_empties() {
        let json = br#"{"items":[
            {"metadata":{"name":"gateway","labels":{"infrastructure":"infrastructure"}}},
            {"metadata":{"name":"gradle-wrappers"}},
            {"metadata":{"name":""}}
        ]}"#;
        let got = resources_from_json(json);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "gateway");
        assert!(got[0].labeled);
        assert_eq!(got[1].name, "gradle-wrappers");
        assert!(!got[1].labeled, "no labels → unlabeled");
    }

    #[test]
    fn resources_from_json_treats_malformed_as_empty() {
        assert!(resources_from_json(b"not json").is_empty());
    }
}
