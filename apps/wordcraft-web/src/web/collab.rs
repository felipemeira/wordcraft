//! Verto: the page side of co-editing (`wordcraft_engine::collab`). The host page carries the
//! updates between the people editing (in Verto, through Firebase); WordCraft only hands them
//! over and takes them in, over the same `postMessage` channel as the host messages.
//!
//! From the host:
//! - `wordcraft:collab-start` `{ role: "seed" }`: share the open document (the first person);
//!   WordCraft answers with `wordcraft:collab-snapshot`. `{ role: "join", bytes }`: join a shared
//!   document from its snapshot (the same file must be open).
//! - `wordcraft:collab-update` `{ bytes }`: someone else's changes.
//! - `wordcraft:collab-snapshot-request`: the whole shared document, for someone joining.
//! - `wordcraft:collab-stop`.
//! - `wordcraft:collab-peers` `{ json: [{ id, name, color: "#rrggbb", block, off }] }`: the others'
//!   carets (the block's id and the character offset), drawn in their colours.
//!
//! To the host: `wordcraft:collab-update` `{ bytes }` with this person's changes,
//! `wordcraft:collab-snapshot` `{ bytes }`, `wordcraft:collab-state` `{ active, error? }`, and
//! `wordcraft:collab-presence` `{ json: { block, off } }` when this person's caret moves.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_ui_egui::WordApp;

enum Incoming {
    Seed,
    Join(Vec<u8>),
    Update(Vec<u8>),
    SnapshotRequest,
    Stop,
    Peers(String),
}

/// Updates and the caret go out at most this often (ms): typing is a command a key.
const SEND_EVERY_MS: f64 = 300.0;

pub struct Link {
    queue: Rc<RefCell<VecDeque<Incoming>>>,
    ctx: egui::Context,
    /// When this person's changes last went out (`Date.now()`).
    sent_at: std::cell::Cell<f64>,
    /// The presence last sent, so it is sent again only when it changes.
    presence: RefCell<Option<String>>,
    _listener: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

fn field(data: &JsValue, key: &str) -> Option<JsValue> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().filter(|v| !v.is_undefined() && !v.is_null())
}

impl Link {
    /// Listens to the host page for co-editing messages; `None` without a same-origin parent.
    pub fn install(ctx: egui::Context) -> Option<Link> {
        let window = web_sys::window()?;
        window.parent().ok()??;
        let origin = window.location().origin().ok()?;
        let queue: Rc<RefCell<VecDeque<Incoming>>> = Rc::default();
        let q = queue.clone();
        let repaint = ctx.clone();
        let listener = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            if ev.origin() != origin {
                return;
            }
            let data = ev.data();
            let Some(kind) = field(&data, "type").and_then(|v| v.as_string()) else { return };
            let bytes = || field(&data, "bytes").map(|v| js_sys::Uint8Array::new(&v).to_vec());
            let message = match kind.as_str() {
                "wordcraft:collab-start" => match field(&data, "role").and_then(|v| v.as_string()).as_deref() {
                    Some("join") => bytes().map(Incoming::Join),
                    _ => Some(Incoming::Seed),
                },
                "wordcraft:collab-update" => bytes().map(Incoming::Update),
                "wordcraft:collab-snapshot-request" => Some(Incoming::SnapshotRequest),
                "wordcraft:collab-stop" => Some(Incoming::Stop),
                "wordcraft:collab-peers" => field(&data, "json").and_then(|v| v.as_string()).map(Incoming::Peers),
                _ => None,
            };
            if let Some(m) = message {
                q.borrow_mut().push_back(m);
                ctx.request_repaint();
            }
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        Some(Link { queue, ctx: repaint, sent_at: std::cell::Cell::new(0.0), presence: RefCell::new(None), _listener: listener })
    }

    /// Takes in what the host sent since the last frame, then hands the host this person's
    /// changes and caret.
    pub fn process(&self, app: &mut WordApp) {
        loop {
            let next = self.queue.borrow_mut().pop_front();
            let Some(message) = next else { break };
            match message {
                Incoming::Seed => match app.session.collab_seed() {
                    Ok(snapshot) => {
                        post("wordcraft:collab-snapshot", Some(&snapshot), None);
                        post_state(true, None);
                    }
                    Err(e) => post_state(false, Some(&e.to_string())),
                },
                Incoming::Join(snapshot) => match app.session.collab_join(&snapshot) {
                    Ok(()) => post_state(true, None),
                    Err(e) => post_state(false, Some(&e.to_string())),
                },
                Incoming::Update(update) => {
                    if let Err(e) = app.session.collab_receive(&update) {
                        log::warn!("{e}");
                        post_state(app.session.collab_active(), Some(&e.to_string()));
                    }
                }
                Incoming::SnapshotRequest => match app.session.collab_snapshot() {
                    Ok(snapshot) => post("wordcraft:collab-snapshot", Some(&snapshot), None),
                    Err(e) => post_state(false, Some(&e.to_string())),
                },
                Incoming::Stop => {
                    app.session.collab_stop();
                    post_state(false, None);
                }
                Incoming::Peers(json) => app.session.collab_peers = peers(&json),
            }
        }
        let now = js_sys::Date::now();
        let wait = SEND_EVERY_MS - (now - self.sent_at.get());
        if wait > 0.0 {
            // Typing: what changed goes out together a moment later.
            self.ctx.request_repaint_after(std::time::Duration::from_millis(wait as u64 + 1));
            return;
        }
        let updates = app.session.collab_outbox();
        if !updates.is_empty() {
            self.sent_at.set(now);
        }
        for update in updates {
            post("wordcraft:collab-update", Some(&update), None);
        }
        let presence = app.session.collab_presence().map(|(block, off)| serde_json::json!({"block": block, "off": off}).to_string());
        if presence != *self.presence.borrow() {
            if let Some(json) = &presence {
                post_json("wordcraft:collab-presence", json);
            }
            *self.presence.borrow_mut() = presence;
        }
    }
}

/// The others, from the host's JSON; entries that don't read are left out.
fn peers(json: &str) -> Vec<wordcraft_engine::collab::Peer> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    list.iter()
        .filter_map(|p| {
            let hex = p["color"].as_str()?.trim_start_matches('#');
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some(wordcraft_engine::collab::Peer {
                id: p["id"].as_str()?.to_string(),
                name: p["name"].as_str().unwrap_or_default().chars().take(40).collect(),
                color: [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8],
                block: p["block"].as_str()?.to_string(),
                off: p["off"].as_u64().unwrap_or(0) as usize,
            })
        })
        .take(50)
        .collect()
}

fn post_json(kind: &str, json: &str) {
    let Some(window) = web_sys::window() else { return };
    let Ok(Some(parent)) = window.parent() else { return };
    let Ok(origin) = window.location().origin() else { return };
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("type"), &JsValue::from_str(kind));
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("json"), &JsValue::from_str(json));
    if let Err(e) = parent.post_message(&message, &origin) {
        log::warn!("co-editing: couldn't post {kind}: {e:?}");
    }
}

fn post_state(active: bool, error: Option<&str>) {
    post("wordcraft:collab-state", None, Some((active, error)));
}

fn post(kind: &str, bytes: Option<&[u8]>, state: Option<(bool, Option<&str>)>) {
    let Some(window) = web_sys::window() else { return };
    let Ok(Some(parent)) = window.parent() else { return };
    let Ok(origin) = window.location().origin() else { return };
    let message = js_sys::Object::new();
    let set = |k: &str, v: &JsValue| {
        let _ = js_sys::Reflect::set(&message, &JsValue::from_str(k), v);
    };
    set("type", &JsValue::from_str(kind));
    if let Some(bytes) = bytes {
        set("bytes", &js_sys::Uint8Array::from(bytes).buffer().into());
    }
    if let Some((active, error)) = state {
        set("active", &JsValue::from_bool(active));
        if let Some(e) = error {
            set("error", &JsValue::from_str(e));
        }
    }
    if let Err(e) = parent.post_message(&message, &origin) {
        log::warn!("co-editing: couldn't post {kind}: {e:?}");
    }
}
