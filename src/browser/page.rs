//! One browser session: the process, its DevTools connection, the page
//! the model drives and the guard that checks every request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::Settings;
use super::address::Policy;
use super::cdp::{Connection, Event, Reader};
use super::process::{self, Process};

/// How long a page may take to load after an action.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an action waits for a navigation to start.
const START_WAIT: Duration = Duration::from_millis(400);
/// The snapshot script ([`Snapshot`]).
const SNAPSHOT_JS: &str = include_str!("snapshot.js");
/// The input guard and the risk rules ([`Browser::attach`]).
pub(super) const GUARD_JS: &str = include_str!("guard.js");
/// The isolated world the guard, the snapshot and the element actions
/// run in (one per frame; the same name gives the same world).
const WORLD: &str = "junees";
/// The risk the guard and the snapshot give a key press or click that
/// goes into a frame of another site (`FRAME_RISK` in guard.js).
pub const FRAME_RISK: &str =
    "goes into a frame from another site, whose fields and buttons cannot be checked";
/// The risk the guard and the snapshot give a click or key press that
/// goes into a closed shadow root (`CLOSED_RISK` in guard.js); the tool
/// passes it on from there, so only the tests name it.
#[cfg(test)]
pub const CLOSED_RISK: &str =
    "goes into a closed shadow root, whose fields and buttons cannot be checked";
/// Marks the element it is called on as the host of a closed shadow root
/// in the guard of the world it runs in.
const MARK_CLOSED: &str =
    "function() { if (globalThis.__juneesGuard) globalThis.__juneesGuard.closed(this); }";
/// The object group of the handles [`Browser::mark_closed_hosts`] takes.
const CLOSED_GROUP: &str = "junees-closed-hosts";
/// The most hosts of closed shadow roots marked one by one per check (two
/// DevTools calls each, per world); a page with more counts as closed
/// everywhere ([`CLOSED_EVERYWHERE`]).
pub const MAX_CLOSED_HOSTS: usize = 256;
/// The most frames told [`CLOSED_EVERYWHERE`] besides the top one.
const MAX_CLOSED_FRAMES: usize = 16;
/// Makes every element count as inside a closed shadow root in the guard
/// of the world it runs in.
const CLOSED_EVERYWHERE: &str =
    "globalThis.__juneesGuard && globalThis.__juneesGuard.closedEverywhere()";

/// Notes of one kind kept until the model looks; older ones are only
/// counted ([`Notes::more`]), so a page cannot fill the result with them.
pub const MAX_NOTES: usize = 20;
/// Characters of one note.
pub const MAX_NOTE: usize = 300;
/// Why the browser shows nothing of a page that came from a private or
/// local address after its name passed the check ([`Policy::check_remote`]).
pub const PRIVATE_PAGE: &str = "not shown: a page came from a private or local address, \
     although its name passed the check before the browser connected (the name's answer \
     changed: DNS rebinding?); the browser left it for a blank page";

/// Things that happened since the model last looked, reported with the
/// next result.
#[derive(Debug, Default)]
pub struct Notes {
    pub blocked: Vec<String>,
    pub dialogs: Vec<String>,
    pub downloads: Vec<String>,
    /// Notes dropped to keep each list at [`MAX_NOTES`].
    pub more: usize,
    /// Page targets opened by the page (popups, `target=_blank`).
    pub popups: Vec<String>,
}

impl Notes {
    pub fn block(&mut self, reason: &str) {
        keep(&mut self.blocked, &mut self.more, reason);
    }

    pub fn dialog(&mut self, line: &str) {
        keep(&mut self.dialogs, &mut self.more, line);
    }

    pub fn download(&mut self, line: &str) {
        keep(&mut self.downloads, &mut self.more, line);
    }
}

/// Adds `line` (cut to [`MAX_NOTE`] characters) to `list`, keeping the
/// newest [`MAX_NOTES`].
fn keep(list: &mut Vec<String>, more: &mut usize, line: &str) {
    if list.len() >= MAX_NOTES {
        list.remove(0);
        *more += 1;
    }
    list.push(cut(line, MAX_NOTE));
}

/// A snapshot: the page's text with numbered elements, and what the tool
/// needs to know about each element.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub title: String,
    pub url: String,
    pub text: String,
    pub refs: Vec<Element>,
    /// The focus is in a frame of another site.
    pub frame_focus: bool,
}

/// What a snapshot says about one numbered element.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub kind: String,
    /// Why typing into it needs approval (`a password field`), or empty.
    pub type_risk: String,
    /// Why clicking it needs approval, or empty.
    pub click_risk: String,
    /// Why pressing Enter in it needs approval, or empty.
    pub enter_risk: String,
    /// Why pressing Space in it needs approval, or empty (a click's risk:
    /// Space sends no form from a field or checkbox).
    pub space_risk: String,
}

pub struct Browser {
    connection: Connection,
    /// The current page: target id and flat session id.
    target: String,
    session: String,
    /// The isolated world the last snapshot ran in, if the page has not
    /// navigated since.
    world: Option<i64>,
    events: broadcast::Sender<Event>,
    notes: Arc<Mutex<Notes>>,
    /// The address of a page the guard found to come from a private or
    /// local address, until an action reports it.
    private_page: Arc<Mutex<Option<String>>>,
    guard: Guard,
    _reader: Reader,
    // Dropped last: kills the browser and removes the profile.
    process: Process,
}

/// The guard task; stopped when dropped.
struct Guard(JoinHandle<()>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Browser {
    /// Starts the browser and opens its page: downloads refused, file
    /// pickers cancelled, and every request checked against `policy`.
    pub async fn start(
        program: &std::path::Path,
        settings: &Settings,
        hidden_env: &[String],
        policy: Policy,
    ) -> Result<Self> {
        let (process, pipes) = process::launch(program, settings, hidden_env)?;
        let (connection, reader, raw_events) =
            Connection::new(pipes.from_browser, pipes.to_browser);
        let (events, _) = broadcast::channel(1024);
        let notes = Arc::new(Mutex::new(Notes::default()));
        let private_page = Arc::new(Mutex::new(None));
        let guard = Guard(tokio::spawn(guard(
            connection.clone(),
            raw_events,
            events.clone(),
            Arc::clone(&notes),
            Arc::clone(&private_page),
            Arc::new(policy),
        )));
        let mut browser = Self {
            connection,
            target: String::new(),
            session: String::new(),
            world: None,
            events,
            notes,
            private_page,
            guard,
            _reader: reader,
            process,
        };
        if let Err(err) = browser.setup().await {
            let tail = browser.process.stderr_tail();
            let hint = if settings.no_sandbox {
                String::new()
            } else {
                "\nIf Chrome's sandbox cannot start here (no user namespaces, running as root), \
                 set browser_no_sandbox (JUNEES_BROWSER_NO_SANDBOX) = on."
                    .to_string()
            };
            bail!(
                "the browser did not start: {err:#}{}{hint}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\nits last output:\n{tail}")
                }
            );
        }
        Ok(browser)
    }

    async fn setup(&mut self) -> Result<()> {
        self.call_browser("Browser.getVersion", json!({})).await?;
        // Before any page loads: every request of every page and frame
        // pauses here until the guard lets it go.
        self.call_browser("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}]}))
            .await?;
        self.call_browser(
            "Browser.setDownloadBehavior",
            json!({"behavior": "deny", "eventsEnabled": true}),
        )
        .await?;
        self.call_browser("Target.setDiscoverTargets", json!({"discover": true}))
            .await?;
        let targets = self.call_browser("Target.getTargets", json!({})).await?;
        let page = targets["targetInfos"]
            .as_array()
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|target| target["type"] == "page")
                    .and_then(|target| target["targetId"].as_str())
            })
            .map(str::to_string);
        let target = match page {
            Some(target) => target,
            None => self
                .call_browser("Target.createTarget", json!({"url": "about:blank"}))
                .await?["targetId"]
                .as_str()
                .context("no page target")?
                .to_string(),
        };
        self.attach(target).await
    }

    async fn attach(&mut self, target: String) -> Result<()> {
        let attached = self
            .call_browser(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .context("no session id")?
            .to_string();
        self.target = target;
        self.session = session;
        self.world = None;
        self.call_page("Page.enable", json!({})).await?;
        // The address each document came from (see `guard`).
        self.call_page("Network.enable", json!({})).await?;
        // The input guard runs in every frame of every document before
        // the page's own scripts, so its listeners come first.
        self.call_page(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": GUARD_JS, "worldName": WORLD, "runImmediately": true}),
        )
        .await?;
        // The window holds some browser frame even headless: lay the page
        // out at exactly the size screenshots and scrolling assume.
        let (width, height) = process::WINDOW;
        self.call_page(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false}),
        )
        .await?;
        self.call_page(
            "Page.setInterceptFileChooserDialog",
            json!({"enabled": true, "cancel": true}),
        )
        .await?;
        Ok(())
    }

    #[cfg(test)]
    pub fn pid(&self) -> i32 {
        self.process.pid()
    }

    #[cfg(test)]
    pub fn profile(&self) -> &std::path::Path {
        self.process.profile()
    }

    /// Whether the browser's side of the pipe is closed (it exited or
    /// crashed).
    pub fn is_closed(&self) -> bool {
        self.connection.closed().is_some()
    }

    /// What happened since the last call, cleared.
    pub fn take_notes(&self) -> Notes {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(PoisonError::into_inner))
    }

    async fn call_browser(&self, method: &str, params: Value) -> Result<Value> {
        self.connection
            .call(method, params, None)
            .await
            .map_err(|err| anyhow!("{method}: {err}"))
    }

    async fn call_page(&self, method: &str, params: Value) -> Result<Value> {
        self.connection
            .call(method, params, Some(&self.session))
            .await
            .map_err(|err| anyhow!("{method}: {err}"))
    }

    /// Opens `url` (already checked) and waits for it to load. The error
    /// says why the page did not open.
    pub async fn navigate(&mut self, url: &str) -> Result<()> {
        let mut events = self.events.subscribe();
        self.world = None;
        // An earlier page left this way is in the notes already.
        self.private_page
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let result = self.call_page("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = result["errorText"].as_str() {
            // The guard left a page from a private address (which ends
            // this navigation too).
            self.refuse_private_page().await?;
            let blocked = self
                .notes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .blocked
                .clone();
            if let Some(reason) = blocked.last() {
                bail!("not opened: {reason}");
            }
            bail!("the page did not open: {error}");
        }
        if result.get("loaderId").is_some() {
            self.wait_for_load(&mut events, true).await;
        }
        Ok(())
    }

    /// Waits for a navigation the last action may have started: up to
    /// [`START_WAIT`] for it to start (`started` says it did), then up to
    /// [`LOAD_TIMEOUT`] for the load event. Switches to a page the action
    /// opened as a new tab.
    async fn wait_for_load(&mut self, events: &mut broadcast::Receiver<Event>, started: bool) {
        let session = self.session.clone();
        let target = self.target.clone();
        let mut loading = started;
        let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
        let start_deadline = tokio::time::Instant::now() + START_WAIT;
        loop {
            let until = if loading { deadline } else { start_deadline };
            let event = match tokio::time::timeout_at(until, events.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
            };
            let ours = event.session.as_deref() == Some(session.as_str());
            match event.method.as_str() {
                "Page.frameStartedLoading" | "Page.frameRequestedNavigation"
                    if ours && event.params["frameId"] == target.as_str() =>
                {
                    loading = true;
                    self.world = None;
                }
                "Page.loadEventFired" if ours => break,
                "Page.frameStoppedLoading"
                    if ours && event.params["frameId"] == target.as_str() =>
                {
                    break;
                }
                _ => {}
            }
        }
        // A link that opened a new tab: go on in the newest one, alone.
        let mut popups = std::mem::take(
            &mut self
                .notes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .popups,
        );
        let newest = popups.pop();
        for other in popups {
            let _ = self
                .call_browser("Target.closeTarget", json!({"targetId": other}))
                .await;
        }
        if let Some(popup) = newest {
            let old = std::mem::take(&mut self.target);
            if self.attach(popup).await.is_ok() {
                let _ = self
                    .call_browser("Target.closeTarget", json!({"targetId": old}))
                    .await;
                let mut events = self.events.subscribe();
                let ready = self.evaluate_in_page("document.readyState", false).await;
                if ready
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|v| v.as_str().map(str::to_string))
                    .as_deref()
                    != Some("complete")
                {
                    Box::pin(self.wait_for_load(&mut events, true)).await;
                }
                self.notes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .dialog("the page opened a new tab; the browser went on in it");
            } else {
                self.target = old;
            }
        }
    }

    /// Takes a snapshot in a new isolated world, which the element
    /// actions then use.
    pub async fn snapshot(&mut self) -> Result<Snapshot> {
        self.refuse_private_page().await?;
        let world = self.new_world().await?;
        // The guard is already there unless this document came without
        // it; then it starts here, late but still before the actions.
        self.evaluate_in(world, &format!("{GUARD_JS}\n;0")).await?;
        if self.mark_closed_hosts(world).await? {
            self.notes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .dialog(&format!(
                    "the page has more than {MAX_CLOSED_HOSTS} closed shadow roots, whose \
                     insides cannot be checked: every click, Enter or Space and typing on it \
                     asks the user first"
                ));
        }
        let value = self.evaluate_in(world, SNAPSHOT_JS).await?;
        // A page that came while the snapshot ran is not read either.
        self.refuse_private_page().await?;
        self.world = Some(world);
        let refs = value["refs"]
            .as_array()
            .map(|refs| {
                refs.iter()
                    .map(|element| Element {
                        kind: text(&element["kind"]),
                        type_risk: text(&element["type_risk"]),
                        click_risk: text(&element["click_risk"]),
                        enter_risk: text(&element["enter_risk"]),
                        space_risk: text(&element["space_risk"]),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Snapshot {
            title: text(&value["title"]),
            url: text(&value["url"]),
            text: text(&value["text"]),
            refs,
            frame_focus: value["frame_focus"].as_bool().unwrap_or(false),
        })
    }

    /// The page's title and address, read in a fresh isolated world.
    pub async fn location(&mut self) -> Result<(String, String)> {
        self.refuse_private_page().await?;
        let world = self.new_world().await?;
        let value = self
            .evaluate_in(world, "({title: document.title, url: location.href})")
            .await?;
        self.refuse_private_page().await?;
        Ok((text(&value["title"]), text(&value["url"])))
    }

    /// Fails when a page came from a private or local address since the
    /// last call (the guard saw its response and sent the tab to a blank
    /// page): makes sure the tab is blank, and nothing of the page is read.
    pub async fn refuse_private_page(&mut self) -> Result<()> {
        let Some(url) = self
            .private_page
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return Ok(());
        };
        self.world = None;
        let _ = self
            .call_page("Page.navigate", json!({"url": "about:blank"}))
            .await;
        bail!(
            "{PRIVATE_PAGE}\n{}",
            super::tool::wrap(&format!("its address: {url}"))
        )
    }

    async fn new_world(&self) -> Result<i64> {
        let world = self
            .call_page(
                "Page.createIsolatedWorld",
                json!({"frameId": self.target, "worldName": WORLD}),
            )
            .await?;
        world["executionContextId"]
            .as_i64()
            .context("no execution context")
    }

    async fn evaluate_in(&self, world: i64, expression: &str) -> Result<Value> {
        let result = self
            .call_page(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "contextId": world,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await
            .map_err(|err| {
                if err.to_string().contains("context") {
                    anyhow!(STALE)
                } else {
                    err
                }
            })?;
        if let Some(details) = result.get("exceptionDetails") {
            bail!("the page script failed: {}", exception(details));
        }
        Ok(result["result"]["value"].clone())
    }

    /// Tells the risk rules which elements host a closed shadow root.
    /// No script in the page can see into one (`shadowRoot` is null, and
    /// events and `elementFromPoint` stop at the host), so a password form
    /// or a buy button inside looks like its host. DevTools sees them: the
    /// hosts DOMSnapshot finds (in every frame of this page's process) are
    /// marked in the guard of `world` (the top frame's, which the snapshot
    /// and the checks run in) and of their own frame, and the rules treat
    /// them like frames of another site. A flat DOMSnapshot rather than
    /// `DOM.getDocument`, which fails on deeply nested pages; a closed
    /// root without nodes in it holds nothing to click. A page with more
    /// than [`MAX_CLOSED_HOSTS`] hosts (which would keep every check busy
    /// for seconds) counts as closed everywhere instead: `true` then.
    async fn mark_closed_hosts(&self, world: i64) -> Result<bool> {
        let snapshot = self
            .call_page("DOMSnapshot.captureSnapshot", json!({"computedStyles": []}))
            .await?;
        let hosts = closed_hosts(&snapshot);
        if hosts.is_empty() {
            return Ok(false);
        }
        if hosts.len() > MAX_CLOSED_HOSTS {
            self.evaluate_in(world, CLOSED_EVERYWHERE).await?;
            let mut frames: Vec<&str> = Vec::new();
            for (frame, _) in &hosts {
                if frames.len() == MAX_CLOSED_FRAMES {
                    break;
                }
                if *frame != self.target && !frames.contains(&frame.as_str()) {
                    frames.push(frame);
                }
            }
            for frame in frames {
                // A frame whose world cannot be made has no guard to tell.
                if let Ok(made) = self
                    .call_page(
                        "Page.createIsolatedWorld",
                        json!({"frameId": frame, "worldName": WORLD}),
                    )
                    .await
                    && let Some(id) = made["executionContextId"].as_i64()
                {
                    let _ = self.evaluate_in(id, CLOSED_EVERYWHERE).await;
                }
            }
            return Ok(true);
        }
        let mut worlds = HashMap::from([(self.target.clone(), world)]);
        for (frame, node) in hosts {
            let mut contexts = vec![world];
            if !worlds.contains_key(&frame) {
                // A frame whose world cannot be made has no guard to tell.
                if let Ok(made) = self
                    .call_page(
                        "Page.createIsolatedWorld",
                        json!({"frameId": frame, "worldName": WORLD}),
                    )
                    .await
                    && let Some(id) = made["executionContextId"].as_i64()
                {
                    worlds.insert(frame.clone(), id);
                }
            }
            if let Some(&own) = worlds.get(&frame)
                && own != world
            {
                contexts.push(own);
            }
            for context in contexts {
                // A node removed since the snapshot is not on the page.
                let Ok(object) = self
                    .call_page(
                        "DOM.resolveNode",
                        json!({"backendNodeId": node, "executionContextId": context,
                               "objectGroup": CLOSED_GROUP}),
                    )
                    .await
                else {
                    continue;
                };
                let Some(id) = object["object"]["objectId"].as_str() else {
                    continue;
                };
                self.call_page(
                    "Runtime.callFunctionOn",
                    json!({"objectId": id, "functionDeclaration": MARK_CLOSED}),
                )
                .await?;
            }
        }
        let _ = self
            .call_page(
                "Runtime.releaseObjectGroup",
                json!({"objectGroup": CLOSED_GROUP}),
            )
            .await;
        Ok(false)
    }

    /// Runs one of the snapshot's element functions that checks an
    /// element before an action (`point(3)`), after marking the hosts of
    /// closed shadow roots the page has now.
    pub async fn element_check(&self, call: &str) -> Result<Value> {
        let world = self.world.context(STALE)?;
        self.mark_closed_hosts(world).await?;
        self.element_call(call).await
    }

    /// Runs one of the snapshot's element functions (`point(3)`).
    pub async fn element_call(&self, call: &str) -> Result<Value> {
        let world = self.world.context(STALE)?;
        let value = self
            .evaluate_in(world, &format!("globalThis.__junees.{call}"))
            .await?;
        match value["error"].as_str() {
            Some("gone") => bail!(STALE),
            _ => Ok(value),
        }
    }

    /// Runs `expression` in the page's own JavaScript (`evaluate`), with a
    /// time limit. The outer error is the browser's (it went away, did not
    /// answer, refused the call); the inner one is what the script threw,
    /// text the page chooses.
    pub async fn evaluate_in_page(
        &self,
        expression: &str,
        await_promise: bool,
    ) -> Result<std::result::Result<Value, String>> {
        let result = self
            .call_page(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": await_promise,
                    "timeout": 10_000,
                }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            return Ok(Err(exception(details)));
        }
        Ok(Ok(result["result"].get("value").cloned().unwrap_or_else(
            || {
                json!(
                    result["result"]["description"]
                        .as_str()
                        .unwrap_or("undefined")
                )
            },
        )))
    }

    /// Moves the mouse to `(x, y)` (the page's mousemove handlers run
    /// before this returns).
    pub async fn move_mouse(&self, x: f64, y: f64) -> Result<()> {
        self.call_page(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        )
        .await?;
        Ok(())
    }

    /// Presses and releases the left button at `(x, y)` (after
    /// [`Self::move_mouse`] there), then waits for what it started.
    pub async fn click_at(&mut self, x: f64, y: f64) -> Result<()> {
        let mut events = self.events.subscribe();
        for (kind, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
            let params = json!({"type": kind, "x": x, "y": y, "button": "left",
                                "clickCount": 1, "buttons": buttons});
            self.call_page("Input.dispatchMouseEvent", params).await?;
        }
        self.wait_for_load(&mut events, false).await;
        Ok(())
    }

    /// Arms the input guard of every frame below the top one with `arm`
    /// (`null` disarms it): an approved action on an element in a frame,
    /// which the top frame's call cannot hand to that frame's guard.
    /// Frames of other sites run elsewhere and are skipped.
    pub async fn arm_frames(&self, arm: &Value) {
        let Ok(tree) = self.call_page("Page.getFrameTree", json!({})).await else {
            return;
        };
        let mut frames = Vec::new();
        let mut todo: Vec<&Value> = tree["frameTree"]["childFrames"]
            .as_array()
            .map(|children| children.iter().collect())
            .unwrap_or_default();
        while let Some(node) = todo.pop() {
            if let Some(id) = node["frame"]["id"].as_str() {
                frames.push(id.to_string());
            }
            if let Some(children) = node["childFrames"].as_array() {
                todo.extend(children);
            }
        }
        let call = format!("globalThis.__juneesGuard && globalThis.__juneesGuard.arm({arm})");
        for frame in frames {
            let Ok(world) = self
                .call_page(
                    "Page.createIsolatedWorld",
                    json!({"frameId": frame, "worldName": WORLD}),
                )
                .await
            else {
                continue;
            };
            if let Some(context) = world["executionContextId"].as_i64() {
                let _ = self.evaluate_in(context, &call).await;
            }
        }
    }

    /// Presses `key` and waits for what it started.
    pub async fn press(&mut self, key: &Key) -> Result<()> {
        let mut events = self.events.subscribe();
        let mut down = json!({
            "type": if key.text.is_empty() { "rawKeyDown" } else { "keyDown" },
            "key": key.key,
            "code": key.code,
            "windowsVirtualKeyCode": key.code_number,
        });
        if !key.text.is_empty() {
            down["text"] = json!(key.text);
        }
        self.call_page("Input.dispatchKeyEvent", down).await?;
        self.call_page(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": key.key, "code": key.code,
                   "windowsVirtualKeyCode": key.code_number}),
        )
        .await?;
        self.wait_for_load(&mut events, false).await;
        Ok(())
    }

    /// Scrolls the page by `delta` pixels (down when positive) with the
    /// mouse wheel in the middle of the window.
    pub async fn scroll(&self, delta: i64) -> Result<()> {
        let (width, height) = process::WINDOW;
        self.call_page(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseWheel", "x": width / 2, "y": height / 2,
                   "deltaX": 0, "deltaY": delta}),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }

    /// Goes one page back in the history; `false` when there is none.
    pub async fn back(&mut self) -> Result<bool> {
        let history = self
            .call_page("Page.getNavigationHistory", json!({}))
            .await?;
        let index = history["currentIndex"].as_i64().unwrap_or(0);
        if index < 1 {
            return Ok(false);
        }
        let Some(entry) = history["entries"].get((index - 1) as usize) else {
            return Ok(false);
        };
        let mut events = self.events.subscribe();
        self.world = None;
        self.call_page(
            "Page.navigateToHistoryEntry",
            json!({"entryId": entry["id"]}),
        )
        .await?;
        self.wait_for_load(&mut events, true).await;
        Ok(true)
    }

    /// A screenshot of the window, PNG, or JPEG when the PNG is larger
    /// than `max_bytes`.
    pub async fn screenshot(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let mut last = 0;
        for params in [
            json!({"format": "png"}),
            json!({"format": "jpeg", "quality": 75}),
            json!({"format": "jpeg", "quality": 40}),
        ] {
            let shot = self.call_page("Page.captureScreenshot", params).await?;
            let bytes = STANDARD
                .decode(shot["data"].as_str().unwrap_or_default())
                .context("the screenshot is not base64")?;
            if bytes.len() <= max_bytes {
                return Ok(bytes);
            }
            last = bytes.len();
        }
        bail!(
            "the screenshot is {} even as a low-quality JPEG (limit {})",
            crate::images::human_bytes(last as u64),
            crate::images::human_bytes(max_bytes as u64)
        )
    }

    /// Closes the browser: asks it to, then kills what is left and
    /// removes the profile.
    pub async fn close(self) {
        let _ = self
            .connection
            .call_within("Browser.close", json!({}), None, Duration::from_secs(2))
            .await;
        let Self { process, guard, .. } = self;
        drop(guard);
        process.finish(Duration::from_secs(2)).await;
    }
}

/// The message when an element number no longer works.
pub const STALE: &str =
    "the page changed since the last snapshot; take a new snapshot and use its numbers";

/// A key `press` knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub key: &'static str,
    pub code: &'static str,
    pub code_number: u32,
    pub text: &'static str,
}

/// The keys `press` takes, by name (any case).
pub fn key(name: &str) -> Option<Key> {
    let (key, code, code_number, text) = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13, "\r"),
        "tab" => ("Tab", "Tab", 9, ""),
        "escape" | "esc" => ("Escape", "Escape", 27, ""),
        "backspace" => ("Backspace", "Backspace", 8, ""),
        "delete" => ("Delete", "Delete", 46, ""),
        "space" | " " => (" ", "Space", 32, " "),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38, ""),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40, ""),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37, ""),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39, ""),
        "pageup" => ("PageUp", "PageUp", 33, ""),
        "pagedown" => ("PageDown", "PageDown", 34, ""),
        "home" => ("Home", "Home", 36, ""),
        "end" => ("End", "End", 35, ""),
        _ => return None,
    };
    Some(Key {
        key,
        code,
        code_number,
        text,
    })
}

/// The names [`key`] takes, for messages.
pub const KEYS: &str = "Enter, Tab, Escape, Backspace, Delete, Space, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, \
     PageUp, PageDown, Home, End";

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// The hosts of closed shadow roots in a `DOMSnapshot.captureSnapshot`
/// answer, as (frame id, backend node id). A node in a closed root whose
/// parent is not in one is a top node of that root, and its parent the
/// host (the snapshot lists no node for the root itself). Hosts inside
/// another closed root are skipped: the outer host covers them.
pub(super) fn closed_hosts(snapshot: &Value) -> Vec<(String, i64)> {
    let strings: Vec<&str> = snapshot["strings"]
        .as_array()
        .map(|all| all.iter().map(|s| s.as_str().unwrap_or("")).collect())
        .unwrap_or_default();
    let string = |index: &Value| {
        index
            .as_u64()
            .and_then(|i| strings.get(i as usize).copied())
            .unwrap_or("")
    };
    let mut hosts = Vec::new();
    for document in snapshot["documents"].as_array().into_iter().flatten() {
        let frame = string(&document["frameId"]).to_string();
        let nodes = &document["nodes"];
        let parents = nodes["parentIndex"].as_array();
        let ids = nodes["backendNodeId"].as_array();
        let (Some(parents), Some(ids)) = (parents, ids) else {
            continue;
        };
        let roots = &nodes["shadowRootType"];
        let closed: std::collections::HashSet<i64> = roots["index"]
            .as_array()
            .into_iter()
            .flatten()
            .zip(roots["value"].as_array().into_iter().flatten())
            .filter(|(_, kind)| string(kind) == "closed")
            .filter_map(|(index, _)| index.as_i64())
            .collect();
        for &node in &closed {
            let Some(parent) = usize::try_from(node)
                .ok()
                .and_then(|i| parents.get(i))
                .and_then(Value::as_i64)
            else {
                continue;
            };
            if closed.contains(&parent) {
                continue;
            }
            let Some(id) = usize::try_from(parent)
                .ok()
                .and_then(|i| ids.get(i))
                .and_then(Value::as_i64)
            else {
                continue;
            };
            if !hosts.iter().any(|(f, h)| *f == frame && *h == id) {
                hosts.push((frame.clone(), id));
            }
        }
    }
    hosts
}

fn exception(details: &Value) -> String {
    details["exception"]["description"]
        .as_str()
        .or_else(|| details["text"].as_str())
        .unwrap_or("an exception")
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Handles the events the browser needs an answer to, then passes every
/// event on to the actions waiting for one: paused requests (checked
/// against `policy`), documents that came from a private or local address
/// although their name passed (left for a blank page, `private_page`
/// set), dialogs (dismissed), downloads (refused by the browser; noted)
/// and new tabs (noted).
async fn guard(
    connection: Connection,
    mut raw: mpsc::UnboundedReceiver<Event>,
    events: broadcast::Sender<Event>,
    notes: Arc<Mutex<Notes>>,
    private_page: Arc<Mutex<Option<String>>>,
    policy: Arc<Policy>,
) {
    while let Some(event) = raw.recv().await {
        match event.method.as_str() {
            "Fetch.requestPaused" => {
                let connection = connection.clone();
                let notes = Arc::clone(&notes);
                let policy = Arc::clone(&policy);
                let params = event.params.clone();
                let session = event.session.clone();
                tokio::spawn(async move {
                    let id = params["requestId"].clone();
                    let url = params["request"]["url"].as_str().unwrap_or_default();
                    let (method, params) = match policy.check(url).await {
                        Ok(()) => ("Fetch.continueRequest", json!({"requestId": id})),
                        Err(reason) => {
                            notes
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .block(&reason);
                            (
                                "Fetch.failRequest",
                                json!({"requestId": id, "errorReason": "BlockedByClient"}),
                            )
                        }
                    };
                    let _ = connection.call(method, params, session.as_deref()).await;
                });
            }
            // Chrome resolves a name again when it connects: a name whose
            // answer changed since `policy.check` (DNS rebinding) shows here,
            // before the page's load event. Set before the event goes on,
            // so the action that waits for the load sees it.
            "Network.responseReceived" if event.params["type"] == "Document" => {
                let response = &event.params["response"];
                let remote = response["remoteIPAddress"].as_str().unwrap_or_default();
                if let Err(ip) = policy.check_remote(remote) {
                    let url = cut(response["url"].as_str().unwrap_or_default(), MAX_NOTE);
                    notes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .dialog(&format!(
                            "a page came from {ip}, a private or local address (DNS \
                             rebinding?); the browser left it for a blank page: {url}"
                        ));
                    *private_page.lock().unwrap_or_else(PoisonError::into_inner) = Some(url);
                    let connection = connection.clone();
                    let session = event.session.clone();
                    tokio::spawn(async move {
                        let _ = connection
                            .call(
                                "Page.navigate",
                                json!({"url": "about:blank"}),
                                session.as_deref(),
                            )
                            .await;
                    });
                }
            }
            "Page.javascriptDialogOpening" => {
                let kind = event.params["type"]
                    .as_str()
                    .unwrap_or("dialog")
                    .to_string();
                let message: String = event.params["message"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect();
                notes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .dialog(&format!(
                        "the page showed a {kind} dialog (dismissed): {message}"
                    ));
                // Leaving the page (beforeunload) goes on; alert, confirm
                // and prompt get \"cancel\".
                let accept = kind == "beforeunload";
                let connection = connection.clone();
                let session = event.session.clone();
                tokio::spawn(async move {
                    let _ = connection
                        .call(
                            "Page.handleJavaScriptDialog",
                            json!({"accept": accept}),
                            session.as_deref(),
                        )
                        .await;
                });
            }
            "Browser.downloadWillBegin" => {
                let url = event.params["url"].as_str().unwrap_or_default();
                notes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .download(&format!("a download was refused: {}", cut(url, 200)));
            }
            "Target.targetCreated"
                if event.params["targetInfo"]["type"] == "page"
                    && event.params["targetInfo"]["openerId"].is_string() =>
            {
                if let Some(id) = event.params["targetInfo"]["targetId"].as_str() {
                    notes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .popups
                        .push(id.to_string());
                }
            }
            _ => {}
        }
        // The actions wait for page events only; a page's requests would
        // crowd them out.
        if !event.method.starts_with("Network.") {
            let _ = events.send(event);
        }
    }
}

/// `text` cut to `max` characters, with `...` when cut.
pub fn cut(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push_str("...");
    }
    out
}
