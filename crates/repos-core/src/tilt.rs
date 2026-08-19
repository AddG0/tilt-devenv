//! The low-level Tilt client (the seam): build UIButtons, apply/delete them via
//! the `tilt` CLI, and stream button clicks from Tilt's apiserver. Knows nothing
//! about *which* buttons a caller shows — that's the caller's concern.
//!
//! It also answers which Tilt a process belongs to, by walking up the process
//! tree: every call has to reach that Tilt's apiserver, and the port deciding
//! that is on its command line rather than anywhere it can be read directly.
//!
//! Buttons are created *unowned* (no Tiltfile ownerReference) so the Tiltfile
//! controller won't reconcile them away. Clicks are delivered by [`watch_clicks`]
//! and handled in-process by the caller, so an action runs in the watching
//! process — no separate command invocation.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// Prefix for a repo's branch-picker button (`repos-branch-<resource>`, one
/// per resource `repos-tiltd` manages). Shared with the CLI: [`branch_managed_resources`]
/// uses it to tell a repo-backed Tilt resource apart from an unrelated one
/// (infra, setup tasks) the same Tiltfile also defines.
pub const BRANCH_BUTTON_PREFIX: &str = "repos-branch-";

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
                icon_svg: String::new(),
                location: Location {
                    component_id: String::new(),
                    component_type: String::new(),
                },
                inputs: Vec::new(),
                disabled: false,
                requires_confirmation: false,
            },
        }
    }

    pub fn icon(mut self, name: &str) -> UiButton {
        self.spec.icon_name = name.to_string();
        self
    }

    /// An inline `<svg>`, which Tilt gives precedence over [`icon`](Self::icon).
    /// The only way to colour a button — Tilt offers no colour option.
    pub fn icon_svg(mut self, svg: &str) -> UiButton {
        self.spec.icon_svg = svg.to_string();
        self
    }

    /// Has Tilt confirm before it delivers the click — declining means no click
    /// arrives at all.
    pub fn requires_confirmation(mut self, requires: bool) -> UiButton {
        self.spec.requires_confirmation = requires;
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

    /// Adds a free-text input the click carries back under `name`. Appends, so
    /// a button can combine this with other inputs (e.g. a branch name plus a
    /// filter dropdown).
    pub fn text_input(mut self, name: &str, label: &str, placeholder: &str) -> UiButton {
        self.spec.inputs.push(UiInput {
            name: name.to_string(),
            label: label.to_string(),
            text: Some(UiText {
                placeholder: placeholder.to_string(),
            }),
            choice: None,
            boolean: None,
        });
        self
    }

    /// Adds a dropdown input the click carries back under `name` (the chosen
    /// string arrives as that input's value). Appends, like [`text_input`](Self::text_input).
    pub fn choice_input(mut self, name: &str, label: &str, choices: Vec<String>) -> UiButton {
        self.spec.inputs.push(UiInput {
            name: name.to_string(),
            label: label.to_string(),
            text: None,
            choice: Some(UiChoice { choices }),
            boolean: None,
        });
        self
    }

    /// Adds a checkbox input the click carries back under `name`. Appends,
    /// like [`text_input`](Self::text_input) — useful for several checkboxes on
    /// one button (a multi-select the Tilt API has no native widget for).
    pub fn bool_input(mut self, name: &str, label: &str, default: bool) -> UiButton {
        self.spec.inputs.push(UiInput {
            name: name.to_string(),
            label: label.to_string(),
            text: None,
            choice: None,
            boolean: Some(UiBool {
                default_value: default,
            }),
        });
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
    #[serde(rename = "iconSVG", skip_serializing_if = "String::is_empty")]
    icon_svg: String,
    location: Location,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<UiInput>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    disabled: bool,
    #[serde(
        rename = "requiresConfirmation",
        skip_serializing_if = "std::ops::Not::not"
    )]
    requires_confirmation: bool,
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
    #[serde(rename = "bool", skip_serializing_if = "Option::is_none")]
    boolean: Option<UiBool>,
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

#[derive(Serialize)]
struct UiBool {
    #[serde(rename = "defaultValue")]
    default_value: bool,
}

/// How far up the process tree to look for `tilt`. Far past the real depth
/// (tilt -> shell -> daemon), but bounded so a cycle or an odd `ps` reply
/// can't spin.
const MAX_ANCESTRY_DEPTH: usize = 64;

/// The pid of the `tilt` this process runs under, for a caller that needs to
/// signal it — see [`crate::supervisor::request_restart`].
pub fn ancestor_pid() -> Option<u32> {
    Some(ancestor()?.pid)
}

/// The command line of the `tilt` this process is a descendant of, or `None`
/// outside one (a CLI run from a shell) — the flags Tilt was actually given,
/// which its environment doesn't reliably reflect.
fn ancestor_command_line() -> Option<String> {
    Some(ancestor()?.args)
}

/// The `tilt` this process descends from.
fn ancestor() -> Option<Proc> {
    find_tilt(std::process::id(), ps_info)
}

/// One process, as `ps` describes it.
#[derive(Debug, Clone, PartialEq)]
struct Proc {
    pid: u32,
    ppid: u32,
    /// The executed file's name, reduced to its basename.
    comm: String,
    /// The full command line, flags included.
    args: String,
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Walks up from `pid` through `info` until it finds a `tilt`, returning it.
/// Ancestry rather than a pidfile: the daemon is spawned by Tilt itself, so the
/// tree already records the relationship and there's no stale file to clean up.
fn find_tilt(pid: u32, info: impl Fn(u32) -> Option<Proc>) -> Option<Proc> {
    let mut pid = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        let p = info(pid)?;
        if is_tilt_proc(&p) {
            return Some(p);
        }
        if p.ppid == 0 || p.ppid == pid {
            return None;
        }
        pid = p.ppid;
    }
    None
}

fn is_tilt_proc(p: &Proc) -> bool {
    if p.comm == "tilt" {
        return true;
    }
    let mut args = p.args.split_whitespace().map(basename);
    matches!(
        (args.next(), args.next()),
        (Some("tilt"), _) | (Some("sh" | "bash" | "zsh" | "dash"), Some("tilt"))
    )
}

/// `pid`'s parent, exec name and command line, via `ps` — portable across Linux
/// and macOS, unlike reading `/proc`. `-ww` so a long command line isn't
/// truncated to the terminal width, losing the flags that are the point of it.
fn ps_info(pid: u32) -> Option<Proc> {
    let out = Command::new("ps")
        .args(["-ww", "-o", "ppid=,comm=,args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps(pid, &String::from_utf8_lossy(&out.stdout))
}

/// Parses one `ps -o ppid=,comm=,args=` line. The exec name can itself be a path
/// (`/nix/store/…/bin/tilt` on macOS), so it's reduced to its basename; the rest
/// of the line is the command line, spaces and all.
fn parse_ps(pid: u32, out: &str) -> Option<Proc> {
    let line = out.lines().next()?.trim();
    let (ppid, rest) = line.split_once(char::is_whitespace)?;
    let (comm, args) = rest.trim().split_once(char::is_whitespace)?;
    Some(Proc {
        pid,
        ppid: ppid.trim().parse().ok()?,
        comm: basename(comm).to_string(),
        args: args.trim().to_string(),
    })
}

/// Tilt's own default web port, which names its context when nothing else does.
const DEFAULT_PORT: u16 = 10350;

static PORT: LazyLock<Option<u16>> =
    LazyLock::new(|| port_for(ancestor_command_line().as_deref(), env_port()));

static PINNED_PORT: OnceLock<u16> = OnceLock::new();

/// Aims every later `tilt` call at `port`, for a caller that knows which Tilt it
/// means but doesn't descend from one — a CLI run from a shell. The first call wins.
pub fn set_apiserver_port(port: u16) {
    // Already set means a caller pinned twice: decided, not failed.
    let _ = PINNED_PORT.set(port);
}

/// The port every `tilt` will be aimed at: a pin from [`set_apiserver_port`],
/// else the Tilt this process descends from. `None` leaves the choice to the
/// environment — a caller that runs under Tilt should report it, since the
/// ancestry walk failed and its calls may reach another apiserver, or a dead one.
pub fn apiserver_port() -> Option<u16> {
    PINNED_PORT.get().copied().or(*PORT)
}

/// A `tilt` aimed at the apiserver of the Tilt this process belongs to.
///
/// The port decides which context the CLI resolves, and Tilt never exports its
/// own — so a port left in the environment outranks the `--port` Tilt was given,
/// and every call lands on another Tilt, or on a dead one. The flag, not the
/// variable: Tilt documents the flag as overriding it.
fn tilt() -> Command {
    let mut cmd = Command::new("tilt");
    if let Some(port) = apiserver_port() {
        cmd.args(["--port", &port.to_string()]);
    }
    cmd
}

/// The port Tilt serves on given its command line, resolved as Tilt resolves
/// it: `--port` beats `TILT_PORT` beats the default. `None` when this process
/// has no Tilt of its own, where the environment is all there is to go on.
fn port_for(tilt_args: Option<&str>, env: Option<u16>) -> Option<u16> {
    let args = tilt_args?;
    Some(port_from_args(args).or(env).unwrap_or(DEFAULT_PORT))
}

fn env_port() -> Option<u16> {
    std::env::var("TILT_PORT").ok()?.trim().parse().ok()
}

/// The last `--port` Tilt itself was given, matching how its flag parser treats
/// a repeated flag. Accepts both `--port 10350` and `--port=10350`, and stops at
/// a bare `--`, after which the flags belong to the Tiltfile.
fn port_from_args(args: &str) -> Option<u16> {
    let mut fields = args.split_whitespace().peekable();
    let mut found = None;
    while let Some(field) = fields.next() {
        if field == "--" {
            break;
        }
        let value = match field.strip_prefix("--port") {
            Some("") => fields.peek().copied(),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        if let Some(port) = value.and_then(|v| v.trim().parse().ok()) {
            found = Some(port);
        }
    }
    found
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
    let mut child = tilt()
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

/// A Tilt UIResource's name.
pub struct Resource {
    pub name: String,
}

/// Every UIResource via `tilt get uiresource -o json`. Erroring here doubles as
/// a "is Tilt up?" check.
pub fn uiresources() -> Result<Vec<Resource>> {
    let out = tilt()
        .args(["get", "uiresource", "-o", "json"])
        .output()
        .context("running `tilt get uiresource`")?;
    check(&out, "tilt get uiresource")?;
    Ok(resources_from_json(&out.stdout))
}

/// Parses a `tilt get uiresource -o json` list into names. Malformed output
/// yields an empty list rather than an error.
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
    }
    serde_json::from_slice::<List>(json)
        .unwrap_or_default()
        .items
        .into_iter()
        .map(|i| Resource {
            name: i.metadata.name,
        })
        .filter(|r| !r.name.is_empty())
        .collect()
}

/// The Tilt resource names `repos-tiltd` manages a branch-picker button for —
/// i.e. backed by one of the registry's repos, as opposed to infra or setup
/// tasks the same Tiltfile also defines. Derived from `tilt get uibutton -o
/// json` rather than resource labels: the daemon doesn't create the actual
/// service resources (the Tiltfile does), so a button it *does* create is the
/// only signal it directly controls.
pub fn branch_managed_resources() -> Result<Vec<String>> {
    let out = tilt()
        .args(["get", "uibutton", "-o", "json"])
        .output()
        .context("running `tilt get uibutton`")?;
    check(&out, "tilt get uibutton")?;
    Ok(branch_managed_from_json(&out.stdout))
}

fn branch_managed_from_json(json: &[u8]) -> Vec<String> {
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
    }
    serde_json::from_slice::<List>(json)
        .unwrap_or_default()
        .items
        .into_iter()
        .filter_map(|i| {
            i.metadata
                .name
                .strip_prefix(BRANCH_BUTTON_PREFIX)
                .map(str::to_string)
        })
        .filter(|r| !r.is_empty())
        .collect()
}

/// Deletes a UIButton by name. A missing button is not an error.
pub fn delete_button(name: &str) -> Result<()> {
    tracing::debug!(name, "tilt delete uibutton");
    let out = tilt()
        .args(["delete", "uibutton", name, "--ignore-not-found"])
        .output()
        .with_context(|| format!("spawning `tilt delete uibutton {name}`"))?;
    check(&out, &format!("tilt delete uibutton {name}"))
}

/// Forces Tilt to (re)run a resource's update command, regardless of its
/// trigger mode. Used to refresh the git-status resource on a git change.
pub fn trigger(resource: &str) -> Result<()> {
    tracing::debug!(resource, "tilt trigger");
    let out = tilt()
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
    #[serde(rename = "bool", default)]
    boolean: Option<WatchBool>,
}

#[derive(Deserialize)]
struct WatchValue {
    value: String,
}

#[derive(Deserialize)]
struct WatchBool {
    value: bool,
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
        } else if let Some(b) = &input.boolean {
            m.insert(input.name.clone(), b.value.to_string());
        }
    }
    m
}

/// One lock over the running child and the stop signal, so a shutdown can't
/// land between spawning a child and recording it — which would leak it.
#[derive(Default)]
struct Watch {
    stop: bool,
    child: Option<std::process::Child>,
}

/// Stops the click stream on drop (used at daemon shutdown).
pub struct ClickWatcher(Arc<Mutex<Watch>>);

impl Drop for ClickWatcher {
    fn drop(&mut self) {
        let mut watch = self.0.lock().unwrap();
        watch.stop = true;
        // Killing it ends the supervisor's read, so it sees `stop` rather than
        // re-establishing the stream.
        if let Some(mut child) = watch.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// How long to wait before re-establishing a dropped stream. Flat, not backed
/// off: the daemon is a Tilt child, so a Tilt that stays down kills it rather
/// than leaving it retrying.
const RESTREAM_DELAY: Duration = Duration::from_secs(1);

/// Streams UIButton clicks from Tilt's apiserver. A click is reported only when
/// a button's `lastClickedAt` advances past what was last seen, so re-applying a
/// button's spec (or a click that predates the daemon) isn't mistaken for a
/// fresh press. The returned [`ClickWatcher`] must be kept alive; dropping it
/// stops the stream.
///
/// `tilt get --watch-only` does not survive forever — a Tiltfile re-execution
/// alone has been observed to end it — so this supervises the child and
/// re-establishes the stream. A press during the gap still arrives: the stream
/// opens by emitting every button as it currently stands, so the reconnect
/// carries a `lastClickedAt` that postdates what was last seen.
///
/// Errors when the apiserver can't be reached at all: a caller that can't watch
/// must say so rather than run on deaf.
pub fn watch_clicks() -> Result<(tokio::sync::mpsc::UnboundedReceiver<Click>, ClickWatcher)> {
    let mut last_seen = seen_at(&buttons_now().context("reading Tilt's buttons to watch them")?);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = Arc::new(Mutex::new(Watch::default()));

    let supervised = watch.clone();
    std::thread::spawn(move || {
        loop {
            if let Err(e) = stream_clicks(&supervised, &mut last_seen, &tx) {
                tracing::error!(error = %format!("{e:#}"), "couldn't start the click stream");
            }
            if supervised.lock().unwrap().stop || tx.is_closed() {
                return;
            }
            std::thread::sleep(RESTREAM_DELAY);
            tracing::info!("re-establishing the click stream");
        }
    });

    Ok((rx, ClickWatcher(watch)))
}

/// Every button as Tilt currently holds it.
fn buttons_now() -> Result<Vec<WatchButton>> {
    let out = tilt()
        .args(["get", "uibutton", "-o", "json"])
        .output()
        .context("spawning `tilt get uibutton`")?;
    check(&out, "tilt get uibutton")?;
    let list: WatchList =
        serde_json::from_slice(&out.stdout).context("parsing `tilt get uibutton` output")?;
    Ok(list.items)
}

/// Each button's `lastClickedAt`, the baseline a click has to postdate.
fn seen_at(buttons: &[WatchButton]) -> HashMap<String, String> {
    buttons
        .iter()
        .map(|b| {
            (
                b.metadata.name.clone(),
                b.status.last_clicked_at.clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// `b`'s press if it postdates `last_seen`, advancing that entry past it.
fn click_of(b: &WatchButton, last_seen: &mut HashMap<String, String>) -> Option<Click> {
    let at = b.status.last_clicked_at.clone().unwrap_or_default();
    if at.is_empty() || last_seen.get(&b.metadata.name) == Some(&at) {
        return None;
    }
    last_seen.insert(b.metadata.name.clone(), at);
    Some(Click {
        button: b.metadata.name.clone(),
        inputs: inputs_of(b),
    })
}

/// Runs one `tilt get --watch-only` until its stream ends, forwarding clicks.
/// Returning is not an error — the caller re-establishes the stream. Errs only
/// when the child won't start.
fn stream_clicks(
    watch: &Mutex<Watch>,
    last_seen: &mut HashMap<String, String>,
    tx: &tokio::sync::mpsc::UnboundedSender<Click>,
) -> Result<()> {
    let mut child = tilt()
        .args(["get", "uibutton", "-o", "json", "--watch-only"])
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning `tilt get --watch-only`")?;
    let stdout = child.stdout.take().expect("stdout was piped");
    {
        let mut watch = watch.lock().unwrap();
        if watch.stop {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        watch.child = Some(child);
    }

    pump_clicks(stdout, last_seen, tx);

    // Reap it, or a dead watch lingers as a zombie for the daemon's whole life.
    // Already taken means [`ClickWatcher`] did it: a shutdown, not a fault.
    if let Some(mut child) = watch.lock().unwrap().child.take() {
        let _ = child.kill();
        match child.wait() {
            Ok(status) => tracing::warn!(%status, "the click stream ended"),
            Err(e) => tracing::warn!(error = %e, "the click stream ended"),
        }
    }
    Ok(())
}

/// Forwards each click in a `--watch-only` stream until it ends or the receiver
/// goes away.
///
/// Frames go through [`serde_json::Value`] rather than straight into a
/// `WatchButton`, which would stop at the first field of the wrong shape —
/// mid-object, desyncing the stream, so one odd frame costs every click after
/// it. A `Value` always consumes its whole frame, leaving only malformed JSON
/// fatal, and nothing later in that stream can recover from it.
fn pump_clicks<R: std::io::Read>(
    reader: R,
    last_seen: &mut HashMap<String, String>,
    tx: &tokio::sync::mpsc::UnboundedSender<Click>,
) {
    for frame in serde_json::Deserializer::from_reader(reader).into_iter::<serde_json::Value>() {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "the click stream desynced");
                return;
            }
        };
        let b: WatchButton = match serde_json::from_value(frame) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "skipping a frame on the click stream that isn't a UIButton");
                continue;
            }
        };
        if b.kind != "UIButton" {
            continue;
        }
        if let Some(click) = click_of(&b, last_seen)
            && tx.send(click).is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `--watch-only` frame for `name`, clicked at `at` (empty = never).
    fn frame(name: &str, at: &str) -> String {
        let clicked = if at.is_empty() {
            "null".to_string()
        } else {
            format!("\"{at}\"")
        };
        format!(
            r#"{{"kind":"UIButton","metadata":{{"name":"{name}"}},"status":{{"lastClickedAt":{clicked}}}}}"#
        )
    }

    fn pump(stream: &str, last_seen: &mut HashMap<String, String>) -> Vec<Click> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pump_clicks(std::io::Cursor::new(stream.to_string()), last_seen, &tx);
        drop(tx);
        let mut clicks = Vec::new();
        while let Ok(c) = rx.try_recv() {
            clicks.push(c);
        }
        clicks
    }

    #[test]
    fn should_keep_streaming_after_a_frame_that_is_not_a_button() {
        // An apiserver `Status`: `status` is a string where a UIButton's is an
        // object, the shape that used to desync the stream.
        let stream = format!(
            r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","message":"too old resource version"}} {}"#,
            frame("repos-profile", "2026-08-13T20:54:15Z")
        );

        let clicks = pump(&stream, &mut HashMap::new());

        assert_eq!(
            clicks.iter().map(|c| c.button.as_str()).collect::<Vec<_>>(),
            vec!["repos-profile"],
            "one unreadable frame must not cost every click that follows it"
        );
    }

    #[test]
    fn should_stop_reading_once_the_frames_desync() {
        let stream = format!(
            "{} {}",
            frame("repos-profile", "2026-08-13T20:54:15Z"),
            r#"{"kind":"UIButton","metadata":{"name":"repos-checkout-all""#
        );

        let clicks = pump(&stream, &mut HashMap::new());

        assert_eq!(
            clicks.len(),
            1,
            "truncated JSON leaves the parser mid-value; only a fresh stream recovers"
        );
    }

    #[test]
    fn should_report_a_button_once_per_press() {
        let mut last_seen = HashMap::new();
        let at = "2026-08-13T20:54:15Z";

        let first = pump(&frame("repos-profile", at), &mut last_seen);
        let repeat = pump(&frame("repos-profile", at), &mut last_seen);
        let pressed_again = pump(
            &frame("repos-profile", "2026-08-13T21:00:00Z"),
            &mut last_seen,
        );

        assert_eq!(first.len(), 1);
        assert!(
            repeat.is_empty(),
            "re-applying a button's spec re-emits it unchanged; that's not a press"
        );
        assert_eq!(pressed_again.len(), 1);
    }

    #[test]
    fn should_ignore_a_button_that_was_never_clicked() {
        let clicks = pump(&frame("repos-profile", ""), &mut HashMap::new());

        assert!(clicks.is_empty());
    }

    #[test]
    fn should_report_a_click_that_landed_while_the_stream_was_down() {
        let unclicked: Vec<WatchButton> =
            serde_json::from_str(&format!("[{}]", frame("repos-profile", ""))).unwrap();
        let mut last_seen = seen_at(&unclicked);

        // A reconnect opens with every button as it now stands, the press
        // included.
        let reconnect = frame("repos-profile", "2026-08-13T20:54:15Z");

        assert_eq!(pump(&reconnect, &mut last_seen).len(), 1);
        assert!(
            pump(&reconnect, &mut last_seen).is_empty(),
            "and the next reconnect's opening frames must not replay it again"
        );
    }

    /// A fake process tree of `(pid, parent pid, exec name, command line)`.
    fn tree(entries: &[(u32, u32, &str, &str)]) -> impl Fn(u32) -> Option<Proc> + use<> {
        let map: HashMap<u32, Proc> = entries
            .iter()
            .map(|(pid, ppid, comm, args)| {
                (
                    *pid,
                    Proc {
                        pid: *pid,
                        ppid: *ppid,
                        comm: comm.to_string(),
                        args: args.to_string(),
                    },
                )
            })
            .collect();
        move |pid| map.get(&pid).cloned()
    }

    fn pid_of(found: Option<Proc>) -> Option<u32> {
        found.map(|p| p.pid)
    }

    #[test]
    fn should_find_the_tilt_process_the_daemon_runs_under() {
        // The real shape: tilt spawns a shell for the serve_cmd, which runs us.
        let procs = tree(&[
            (300, 200, "repos-tiltd", "repos-tiltd"),
            (200, 100, "sh", "sh -c repos-tiltd"),
            (100, 1, "tilt", "tilt up --port 10352"),
            (1, 0, "init", "init"),
        ]);
        assert_eq!(pid_of(find_tilt(300, procs)), Some(100));
    }

    #[test]
    fn should_return_none_when_no_ancestor_is_tilt() {
        let procs = tree(&[
            (300, 200, "repos-tiltd", "repos-tiltd"),
            (200, 1, "zsh", "zsh"),
            (1, 0, "init", "init"),
        ]);
        assert_eq!(pid_of(find_tilt(300, procs)), None);
    }

    #[test]
    fn should_stop_rather_than_loop_on_a_self_parenting_process() {
        let procs = tree(&[(300, 300, "weird", "weird")]);
        assert_eq!(pid_of(find_tilt(300, procs)), None);
    }

    #[test]
    fn should_return_none_when_a_pid_vanishes_mid_walk() {
        // Processes exit while we're walking; a gap must end the walk, not panic.
        let procs = tree(&[(300, 200, "repos-tiltd", "repos-tiltd")]);
        assert_eq!(pid_of(find_tilt(300, procs)), None);
    }

    #[test]
    fn should_find_a_tilt_started_through_a_wrapper_script() {
        let procs = tree(&[
            (300, 200, "repos-tiltd", "repos-tiltd"),
            (
                200,
                1,
                "tilt",
                "/bin/sh /usr/local/bin/tilt up --port 10352",
            ),
            (1, 0, "init", "init"),
        ]);
        let found = find_tilt(300, procs).unwrap();
        assert_eq!(found.pid, 200);
        assert!(found.args.contains("--port 10352"));
    }

    #[test]
    fn should_find_a_tilt_script_when_ps_reports_the_interpreter() {
        let procs = tree(&[
            (300, 200, "repos-tiltd", "repos-tiltd"),
            (
                200,
                1,
                "sh",
                "/bin/sh /nix/store/fake/bin/tilt up --port 10352",
            ),
            (1, 0, "init", "init"),
        ]);
        let found = find_tilt(300, procs).unwrap();
        assert_eq!(found.pid, 200);
        assert!(found.args.contains("--port 10352"));
    }

    #[test]
    fn should_not_mistake_a_command_merely_mentioning_tilt_for_tilt() {
        let procs = tree(&[
            (300, 200, "repos-tiltd", "repos-tiltd"),
            (200, 1, "nvim", "nvim /repo/tilt"),
            (1, 0, "init", "init"),
        ]);
        assert_eq!(pid_of(find_tilt(300, procs)), None);
    }

    #[test]
    fn should_parse_a_ps_line_into_parent_exec_name_and_command_line() {
        assert_eq!(
            parse_ps(9, "  1234 tilt   tilt up --port 10352\n"),
            Some(Proc {
                pid: 9,
                ppid: 1234,
                comm: "tilt".to_string(),
                args: "tilt up --port 10352".to_string(),
            })
        );
    }

    #[test]
    fn should_reduce_a_ps_exec_path_to_its_basename() {
        // macOS `ps -o comm=` prints the full executable path.
        let got = parse_ps(9, " 42 /nix/store/abc-tilt-0.35/bin/tilt tilt up\n").unwrap();
        assert_eq!(got.comm, "tilt");
    }

    #[test]
    fn should_treat_unparseable_ps_output_as_no_parent() {
        assert_eq!(parse_ps(9, ""), None);
        assert_eq!(parse_ps(9, "nonsense\n"), None);
    }

    #[test]
    fn should_take_the_port_tilt_was_given_over_the_one_in_the_environment() {
        // The failure this exists for: a shell exporting a port from an older
        // Tilt, while the running one was started with another.
        assert_eq!(
            port_for(Some("tilt up --port 10352"), Some(10351)),
            Some(10352)
        );
    }

    #[test]
    fn should_fall_back_to_the_environment_when_tilt_was_given_no_port() {
        assert_eq!(port_for(Some("tilt up"), Some(10351)), Some(10351));
    }

    #[test]
    fn should_fall_back_to_tilts_default_when_nothing_names_a_port() {
        assert_eq!(port_for(Some("tilt up"), None), Some(DEFAULT_PORT));
    }

    #[test]
    fn should_leave_the_environment_alone_outside_a_tilt() {
        assert_eq!(
            port_for(None, Some(10351)),
            None,
            "a CLI run from a shell has no Tilt of its own to read a flag from"
        );
    }

    #[test]
    fn should_read_a_port_written_either_way() {
        assert_eq!(port_from_args("tilt up --port 10352"), Some(10352));
        assert_eq!(port_from_args("tilt up --port=10352"), Some(10352));
    }

    #[test]
    fn should_take_the_last_port_as_tilts_flag_parser_does() {
        assert_eq!(port_from_args("tilt up --port 1 --port=2"), Some(2));
    }

    #[test]
    fn should_ignore_a_port_the_tiltfile_was_given() {
        // `tilt up --port A -- --port B` hands B to the Tiltfile's own config
        // parser; only A is the server's.
        assert_eq!(
            port_from_args("tilt up --port 10352 -- --port 9999"),
            Some(10352)
        );
        assert_eq!(port_from_args("tilt up -- --port 9999"), None);
    }

    #[test]
    fn should_find_no_port_in_a_flag_that_merely_starts_like_one() {
        assert_eq!(port_from_args("tilt up --portal 10352"), None);
        assert_eq!(port_from_args("tilt up --port"), None);
        assert_eq!(port_from_args("tilt up --port=nonsense"), None);
    }

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
    fn inputs_of_reads_bool_checkboxes() {
        let raw = r#"{"status":{"inputs":[
            {"name":"frontend","bool":{"value":true}},
            {"name":"backend","bool":{"value":false}}
        ]}}"#;
        let b: WatchButton = serde_json::from_str(raw).unwrap();
        let got = inputs_of(&b);
        assert_eq!(got["frontend"], "true");
        assert_eq!(got["backend"], "false");
    }

    #[test]
    fn bool_input_serializes_as_a_defaultvalue_checkbox() {
        let btn = UiButton::new("b".to_string(), "text".to_string())
            .bool_input("frontend", "Frontend", true)
            .bool_input("backend", "Backend", false);
        let v = serde_json::to_value(&btn).unwrap();
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["name"], "frontend");
        assert_eq!(inputs[0]["bool"]["defaultValue"], true);
        assert_eq!(inputs[1]["bool"]["defaultValue"], false);
    }

    #[test]
    fn icon_svg_and_requires_confirmation_serialize_under_tilts_names() {
        let btn = UiButton::new("b".to_string(), "text".to_string())
            .icon_svg("<svg/>")
            .requires_confirmation(true);
        let v = serde_json::to_value(&btn).unwrap();
        assert_eq!(v["spec"]["iconSVG"], "<svg/>");
        assert_eq!(v["spec"]["requiresConfirmation"], true);
    }

    #[test]
    fn icon_svg_and_requires_confirmation_are_omitted_when_unset() {
        // iconSVG wins over iconName, so an empty-but-present one would blank
        // out every Material-icon button we have.
        let btn = UiButton::new("b".to_string(), "text".to_string()).icon("cloud_download");
        let v = serde_json::to_value(&btn).unwrap();
        assert!(v["spec"].get("iconSVG").is_none());
        assert!(v["spec"].get("requiresConfirmation").is_none());
        assert_eq!(v["spec"]["iconName"], "cloud_download");
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
    fn resources_from_json_reads_names_and_drops_empties() {
        let json = br#"{"items":[
            {"metadata":{"name":"gateway"}},
            {"metadata":{"name":""}}
        ]}"#;
        let got = resources_from_json(json);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "gateway");
    }

    #[test]
    fn resources_from_json_treats_malformed_as_empty() {
        assert!(resources_from_json(b"not json").is_empty());
    }

    #[test]
    fn branch_managed_from_json_strips_the_prefix_and_drops_unrelated_buttons() {
        let json = br#"{"items":[
            {"metadata":{"name":"repos-branch-auth-service"}},
            {"metadata":{"name":"repos-pull-auth-service"}},
            {"metadata":{"name":"repos-checkout-all"}}
        ]}"#;
        assert_eq!(branch_managed_from_json(json), vec!["auth-service"]);
    }

    #[test]
    fn branch_managed_from_json_treats_malformed_as_empty() {
        assert!(branch_managed_from_json(b"not json").is_empty());
    }
}
