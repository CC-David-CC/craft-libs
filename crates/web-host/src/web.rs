//! The bridge an app's web shell calls: find the host, hear its messages, post the app's.
//!
//! ```ignore
//! if let Some(host) = craft_web_host::web::Host::connect("pdfcraft", &canvas) {
//!     host.listen(move |message| inbox.push(message));
//!     host.post_ready(env!("CARGO_PKG_VERSION"), false);
//! }
//! ```

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use web_sys::js_sys;

use crate::{
    Inbox, Incoming, MAX_SAVE_ERROR_CHARS, MAX_SAVE_ID_CHARS, Mode, OpenResult, Outgoing, SaveContext, SaveFailureKind, VERSION, file_name,
    message_type,
};

/// A file the host passes in, not yet copied into the app's memory: check its size first.
pub enum FileData {
    /// `bytes`: an `ArrayBuffer` or `Uint8Array`, viewed in place.
    Bytes(js_sys::Uint8Array),
    /// `file`: a `File` or `Blob`, for the app to read in slices.
    Blob(web_sys::Blob),
}

impl FileData {
    /// The file's length in bytes, from the data itself (a host's `size` field is not trusted).
    pub fn size(&self) -> f64 {
        match self {
            Self::Bytes(array) => f64::from(array.length()),
            Self::Blob(blob) => blob.size(),
        }
    }
}

/// A message from the host.
pub enum Message {
    /// What the host supports; `handoff`: it routes files the app hands off.
    Host { version: String, handoff: bool },
    /// Open a document as File › Open would. `name` is already cleaned ([`crate::file_name`]).
    Open { name: String, data: FileData },
    /// An open carrying a validated correlation id; completion must name the resulting editor document.
    IdentifiedOpen { id: String, name: String, data: FileData },
    /// The save with this `id` is stored.
    Saved { id: String },
    /// A correlated, explicit non-confirmed outcome. Unknown may have committed; never replay it blindly.
    SaveFailed { id: String, outcome: SaveFailureKind, error: String },
    /// The host gave a handed-off file to the app that edits it.
    HandoffDone { name: String, app: String },
}

enum Transport {
    /// The same-origin parent window; only it is heard and answered.
    Parent { window: web_sys::Window, parent: web_sys::Window, origin: String },
    /// The app's canvas; messages are `CustomEvent`s on it.
    Page { canvas: web_sys::EventTarget },
}

/// The connection to the page hosting the app.
pub struct Host {
    app: &'static str,
    transport: Transport,
    dirty: Cell<Option<bool>>,
}

impl Host {
    /// The host for `app` (its id, as in `<app>:ready`) running on `canvas`, or `None` when the
    /// app is not hosted: no `?host=parent` and no `data-host="page"`, no framing parent, or a
    /// parent on another origin (messages only go to this origin, so the browser would drop them).
    pub fn connect(app: &'static str, canvas: &web_sys::Element) -> Option<Self> {
        let window = web_sys::window()?;
        let query = window.location().search().unwrap_or_default();
        let transport = match Mode::detect(&query, canvas.get_attribute("data-host").as_deref())? {
            Mode::Parent => {
                let parent = window.parent().ok().flatten()?;
                if js_sys::Object::is(&parent, &window) {
                    return None;
                }
                let origin = window.location().origin().ok()?;
                // Reading another origin's location throws, which is `None` too.
                if parent.location().origin().ok()? != origin {
                    return None;
                }
                Transport::Parent { window, parent, origin }
            }
            Mode::Page => Transport::Page { canvas: canvas.clone().into() },
        };
        Some(Self { app, transport, dirty: Cell::new(None) })
    }

    pub fn mode(&self) -> Mode {
        match self.transport {
            Transport::Parent { .. } => Mode::Parent,
            Transport::Page { .. } => Mode::Page,
        }
    }

    /// Hear the host's messages for this app. The listener lives as long as the page. The
    /// listeners are `Fn`: a page host can dispatch an event while one is still running (a
    /// `saved` for the `save` the handler just posted), and wasm-bindgen refuses to re-enter an
    /// `FnMut` closure before its body runs, so the message would never reach the queue.
    pub fn listen(&self, handler: impl FnMut(Message) + 'static) {
        let app = self.app;
        let inbox = Rc::new(Inbox::new(handler));
        match &self.transport {
            Transport::Parent { window, parent, origin } => {
                let (parent, origin, inbox) = (parent.clone(), origin.clone(), inbox.clone());
                let on_message = Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                    if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                        return;
                    }
                    let data = event.data();
                    let Some(kind) = string(&data, "type").and_then(|t| Incoming::parse(app, &t)) else { return };
                    if let Some(message) = read(kind, &data) {
                        inbox.deliver(message);
                    }
                });
                if window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
                    on_message.forget();
                }
            }
            Transport::Page { canvas } => {
                for kind in Incoming::ALL {
                    let inbox = inbox.clone();
                    let on_event = Closure::<dyn Fn(web_sys::CustomEvent)>::new(move |event: web_sys::CustomEvent| {
                        if let Some(message) = read(kind, &event.detail()) {
                            inbox.deliver(message);
                        }
                    });
                    let name = message_type(app, kind.kind());
                    if canvas.add_event_listener_with_callback(&name, on_event.as_ref().unchecked_ref()).is_ok() {
                        on_event.forget();
                    }
                }
            }
        }
    }

    /// The app is listening: send a document now. `app_version` is the app's own release;
    /// `reads_files`: it takes a `File`/`Blob` and reads it in slices.
    pub fn post_ready(&self, app_version: &str, reads_files: bool) -> Result<(), String> {
        self.post(Outgoing::Ready, &ready_fields(app_version, reads_files, false), None)
    }

    /// Advertise only after wiring identified opens to their actual successful/error outcomes.
    pub fn post_ready_with_open_results(&self, app_version: &str, reads_files: bool) -> Result<(), String> {
        self.post(Outgoing::Ready, &ready_fields(app_version, reads_files, true), None)
    }

    /// Report successful native adoption, never merely receipt or dispatch of bytes.
    pub fn post_opened(&self, result: &OpenResult<'_>) -> Result<(), String> {
        self.post(Outgoing::Opened, &opened_fields(result)?, None)
    }

    /// Unsaved changes appeared or went away. Repeats of the last state reported are not sent.
    /// The new state is recorded before the send: a page host can report another state while the
    /// event is still dispatching, and that later report wins. A failed send is forgotten.
    pub fn post_dirty(&self, dirty: bool) {
        crate::note_dirty(&self.dirty, dirty, || self.post(Outgoing::Dirty, &[("dirty", dirty.into())], None).is_ok());
    }

    /// Dispatch bytes, optionally correlated by id. Ok means delivery acceptance, not a save.
    /// Account-backed document saves should use `post_identified_save` and wait for a result.
    pub fn post_save(&self, name: &str, bytes: &[u8], id: Option<&str>, save_as: bool) -> Result<(), String> {
        let packet = save_packet(name, bytes, id, save_as, None)?;
        self.post(Outgoing::Save, &packet.fields, Some(&packet.buffer))
    }

    /// Dispatch a serialized editor version with explicit identity and document/export intent.
    /// Neither this context nor the same-origin transport is an authorization grant.
    pub fn post_identified_save(&self, name: &str, bytes: &[u8], save_as: bool, context: &SaveContext<'_>) -> Result<(), String> {
        let packet = save_packet(name, bytes, None, save_as, Some(context))?;
        self.post(Outgoing::Save, &packet.fields, Some(&packet.buffer))
    }

    /// The app couldn't start, couldn't take a document, or a save went unconfirmed (`id`).
    pub fn post_failed(&self, error: &str, id: Option<&str>) -> Result<(), String> {
        let mut fields = vec![("error", JsValue::from(error))];
        if let Some(id) = id {
            fields.push(("id", id.into()));
        }
        self.post(Outgoing::Failed, &fields, None)
    }

    /// Pass the host a file another app edits; it answers [`Message::HandoffDone`] or, when no
    /// app it hosts takes the type, `failed`. Without a host, tell the user which app the file
    /// needs instead.
    pub fn post_handoff(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        self.post(Outgoing::Handoff, &[("name", file_name(Some(name), "document").into()), ("bytes", buffer.clone().into())], Some(&buffer))
    }

    fn post(&self, kind: Outgoing, fields: &[(&str, JsValue)], transfer: Option<&js_sys::ArrayBuffer>) -> Result<(), String> {
        let message_type = message_type(self.app, kind.kind());
        let error = |e: JsValue| e.as_string().unwrap_or_else(|| format!("{e:?}"));
        match &self.transport {
            Transport::Parent { parent, origin, .. } => {
                let message = object(&[("type", message_type.as_str().into())], fields)?;
                match transfer {
                    Some(buffer) => parent.post_message_with_transfer(&message, origin, &js_sys::Array::of1(buffer)),
                    None => parent.post_message(&message, origin),
                }
                .map_err(error)
            }
            Transport::Page { canvas } => {
                let init = web_sys::CustomEventInit::new();
                init.set_detail(&object(&[], fields)?.into());
                let event = web_sys::CustomEvent::new_with_event_init_dict(&message_type, &init).map_err(error)?;
                canvas.dispatch_event(&event).map(|_| ()).map_err(error)
            }
        }
    }
}

fn ready_fields(app_version: &str, reads_files: bool, open_results: bool) -> Vec<(&'static str, JsValue)> {
    let mut fields = vec![("version", VERSION.into()), ("appVersion", app_version.into()), ("file", reads_files.into())];
    if open_results {
        fields.push(("openResult", true.into()));
    }
    fields
}

fn opened_fields(result: &OpenResult<'_>) -> Result<Vec<(&'static str, JsValue)>, String> {
    result.validate().map_err(str::to_string)?;
    Ok(vec![
        ("id", result.id.into()),
        ("documentId", result.document_id.into()),
        ("revision", result.revision.into()),
        ("sourceRetained", result.source_retained.into()),
    ])
}

struct SavePacket {
    buffer: js_sys::ArrayBuffer,
    fields: Vec<(&'static str, JsValue)>,
}

fn save_packet(name: &str, bytes: &[u8], id: Option<&str>, save_as: bool, context: Option<&SaveContext<'_>>) -> Result<SavePacket, String> {
    if let Some(context) = context {
        context.validate().map_err(str::to_string)?;
    }
    let buffer = js_sys::Uint8Array::from(bytes).buffer();
    let mut fields = vec![("name", file_name(Some(name), "document").into()), ("bytes", buffer.clone().into()), ("saveAs", save_as.into())];
    if let Some(id) = context.map(|context| context.id).or(id) {
        fields.push(("id", id.into()));
    }
    if let Some(context) = context {
        fields.extend([
            ("documentId", context.document_id.into()),
            ("revision", context.revision.into()),
            ("operation", context.operation.into()),
            ("documentSave", context.document_save.into()),
        ]);
    }
    Ok(SavePacket { buffer, fields })
}

/// Bound JS strings before allocating a Rust copy (UTF-16 has at most two units per scalar).
fn bounded_string(data: &JsValue, key: &str, max: usize) -> Option<String> {
    bounded_string_value(get(data, key), max)
}

fn bounded_string_value(value: JsValue, max: usize) -> Option<String> {
    if !value.is_string() {
        return None;
    }
    let js = js_sys::JsString::from(value);
    if usize::try_from(js.length()).ok()? > max.saturating_mul(2) {
        return None;
    }
    let text = js.as_string()?;
    (text.chars().count() <= max).then_some(text)
}

/// The message a host's object describes, or `None` when a required field is missing.
fn read(kind: Incoming, data: &JsValue) -> Option<Message> {
    Some(match kind {
        Incoming::Host => Message::Host {
            version: string(data, "version").unwrap_or_else(|| VERSION.to_string()),
            handoff: get(data, "handoff").as_bool() == Some(true),
        },
        Incoming::Open => {
            let raw_id = get(data, "id");
            let id = if raw_id.is_undefined() {
                None
            } else {
                let id = bounded_string_value(raw_id, MAX_SAVE_ID_CHARS)?;
                if !crate::save::valid_save_id(&id) {
                    return None;
                }
                Some(id)
            };
            let name = file_name(string(data, "name").as_deref(), "document");
            let data = file_data(data)?;
            match id {
                Some(id) => Message::IdentifiedOpen { id, name, data },
                None => Message::Open { name, data },
            }
        }
        Incoming::Saved => Message::Saved { id: string(data, "id")? },
        Incoming::SaveFailed => {
            let id = bounded_string(data, "id", MAX_SAVE_ID_CHARS)?;
            if !crate::save::valid_save_id(&id) {
                return None;
            }
            let outcome = SaveFailureKind::parse(&bounded_string(data, "outcome", 16)?)?;
            let error = bounded_string(data, "error", MAX_SAVE_ERROR_CHARS)?;
            Message::SaveFailed { id, outcome, error }
        }
        Incoming::HandoffDone => {
            Message::HandoffDone { name: string(data, "name").unwrap_or_default(), app: string(data, "app").unwrap_or_default() }
        }
    })
}

/// `file` (a `Blob`; its `size` field is ignored for the Blob's own) or `bytes` (an `ArrayBuffer` or `Uint8Array`), not copied.
fn file_data(data: &JsValue) -> Option<FileData> {
    let file = get(data, "file");
    if let Some(blob) = file.dyn_ref::<web_sys::Blob>() {
        return Some(FileData::Blob(blob.clone()));
    }
    let bytes = get(data, "bytes");
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(FileData::Bytes(array.clone()))
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(FileData::Bytes(js_sys::Uint8Array::new(&bytes)))
    } else {
        None
    }
}

fn object(head: &[(&str, JsValue)], fields: &[(&str, JsValue)]) -> Result<js_sys::Object, String> {
    let m = js_sys::Object::new();
    for (key, value) in head.iter().chain(fields) {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).map_err(|_| "couldn't build the message".to_string())?;
    }
    Ok(m)
}

fn get(data: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    get(data, key).as_string()
}

/// Node tests for the parsing of a host's objects: wrong shapes, missing fields, and sizes the
/// host states against the data it sends. No browser; run with `wasm-bindgen-test-runner`.
#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn obj(fields: &[(&str, JsValue)]) -> JsValue {
        object(&[], fields).expect("an object").into()
    }

    fn buffer(len: u32) -> js_sys::ArrayBuffer {
        js_sys::ArrayBuffer::new(len)
    }

    #[wasm_bindgen_test]
    fn identified_open_requires_valid_id_and_does_not_fall_back_to_untracked_open() {
        let data = obj(&[("id", "load-1".into()), ("name", "a.deckcraft".into()), ("bytes", buffer(3).into())]);
        assert!(
            matches!(read(Incoming::Open,&data),Some(Message::IdentifiedOpen {id,name,data}) if id=="load-1"&&name=="a.deckcraft"&&data.size()==3.0)
        );
        for id in [JsValue::NULL, 7.into(), "".into(), "bad\nid".into(), "x".repeat(MAX_SAVE_ID_CHARS + 1).into()] {
            assert!(read(Incoming::Open, &obj(&[("id", id), ("bytes", buffer(3).into())])).is_none());
        }
        assert!(matches!(read(Incoming::Open, &obj(&[("bytes", buffer(3).into())])), Some(Message::Open { .. })));
    }

    #[wasm_bindgen_test]
    fn opened_result_carries_actual_identity_and_explicit_source_retention() {
        for source_retained in [true, false] {
            let result = OpenResult { id: "load-1", document_id: "doc-7", revision: "rev-4", source_retained };
            let data = obj(&opened_fields(&result).expect("valid opened result"));
            assert_eq!(string(&data, "id").as_deref(), Some("load-1"));
            assert_eq!(string(&data, "documentId").as_deref(), Some("doc-7"));
            assert_eq!(string(&data, "revision").as_deref(), Some("rev-4"));
            assert_eq!(get(&data, "sourceRetained").as_bool(), Some(source_retained));
            assert!(opened_fields(&OpenResult { document_id: "", ..result }).is_err());
        }
    }

    #[wasm_bindgen_test]
    fn correlated_open_advertisement_is_opt_in() {
        assert!(get(&obj(&ready_fields("1.0", false, false)), "openResult").is_undefined());
        assert_eq!(get(&obj(&ready_fields("1.0", false, true)), "openResult").as_bool(), Some(true));
    }

    #[wasm_bindgen_test]
    fn identified_saves_capture_context_and_preserve_bytes() {
        let context = SaveContext { id: "request-2", document_id: "doc-9", revision: "17", operation: "file.exportPdf", document_save: false };
        let packet = save_packet("report.pdf", &[0, 12, 255], None, true, Some(&context)).expect("valid context");
        let data = obj(&packet.fields);
        for (key, value) in [("id", "request-2"), ("documentId", "doc-9"), ("revision", "17"), ("operation", "file.exportPdf")] {
            assert_eq!(string(&data, key).as_deref(), Some(value));
        }
        assert_eq!(get(&data, "documentSave").as_bool(), Some(false));
        assert_eq!(get(&data, "saveAs").as_bool(), Some(true));
        assert_eq!(js_sys::Uint8Array::new(&packet.buffer).to_vec(), vec![0, 12, 255]);
        assert!(save_packet("report.pdf", &[], None, false, Some(&SaveContext { id: "", ..context })).is_err());
    }

    #[wasm_bindgen_test]
    fn legacy_saves_do_not_invent_document_context() {
        let packet = save_packet("a.txt", &[1], Some("legacy"), false, None).expect("legacy save");
        let data = obj(&packet.fields);
        assert_eq!(string(&data, "id").as_deref(), Some("legacy"));
        for key in ["documentId", "revision", "operation", "documentSave"] {
            assert!(get(&data, key).is_undefined());
        }
    }

    #[wasm_bindgen_test]
    fn failure_messages_require_explicit_bounded_outcomes() {
        let failure = |id: JsValue, outcome: JsValue, error: JsValue| obj(&[("id", id), ("outcome", outcome), ("error", error)]);
        for kind in [SaveFailureKind::Failed, SaveFailureKind::Cancelled, SaveFailureKind::Conflict, SaveFailureKind::Unknown] {
            assert!(matches!(read(Incoming::SaveFailed, &failure("r1".into(), kind.as_str().into(), "diagnostic".into())),
                Some(Message::SaveFailed { id, outcome, error }) if id == "r1" && outcome == kind && error == "diagnostic"));
        }
        for invalid in [JsValue::UNDEFINED, 3.into(), "".into(), "saved".into(), "timeout".into(), "UNKNOWN".into()] {
            assert!(read(Incoming::SaveFailed, &failure("r1".into(), invalid, "".into())).is_none());
        }
        for invalid in [JsValue::UNDEFINED, 3.into(), "".into(), "a\nb".into(), "x".repeat(MAX_SAVE_ID_CHARS + 1).into()] {
            assert!(read(Incoming::SaveFailed, &failure(invalid, "failed".into(), "".into())).is_none());
        }
        for invalid in [JsValue::UNDEFINED, 3.into(), "x".repeat(MAX_SAVE_ERROR_CHARS + 1).into()] {
            assert!(read(Incoming::SaveFailed, &failure("r1".into(), "failed".into(), invalid)).is_none());
        }
        let unicode = "😀".repeat(MAX_SAVE_ERROR_CHARS);
        assert!(read(Incoming::SaveFailed, &failure("r1".into(), "unknown".into(), unicode.into())).is_some());
        assert!(read(Incoming::SaveFailed, &JsValue::NULL).is_none());
    }

    #[wasm_bindgen_test]
    fn an_open_without_bytes_or_a_blob_is_dropped() {
        for data in [
            obj(&[("name", "a.pdf".into())]),
            obj(&[("name", "a.pdf".into()), ("bytes", "text".into())]),
            obj(&[("name", "a.pdf".into()), ("bytes", 42.into())]),
            obj(&[("name", "a.pdf".into()), ("file", "not a blob".into())]),
            JsValue::NULL,
            JsValue::UNDEFINED,
            JsValue::from_str("a string"),
            JsValue::from_f64(7.0),
        ] {
            assert!(read(Incoming::Open, &data).is_none());
        }
    }

    #[wasm_bindgen_test]
    fn an_open_takes_bytes_as_an_array_buffer_or_a_uint8_array() {
        let ab = buffer(3);
        let array = js_sys::Uint8Array::new_with_length(5);
        for (bytes, len) in [(JsValue::from(ab), 3.0), (JsValue::from(array), 5.0)] {
            let Some(Message::Open { data, .. }) = read(Incoming::Open, &obj(&[("name", "a.pdf".into()), ("bytes", bytes)])) else {
                panic!("an open with bytes is read");
            };
            assert_eq!(data.size(), len);
        }
    }

    #[wasm_bindgen_test]
    fn an_open_without_a_name_or_with_a_hostile_one_gets_a_clean_name() {
        let named = |name: Option<&str>| {
            let mut fields = vec![("bytes", JsValue::from(buffer(1)))];
            if let Some(n) = name {
                fields.push(("name", n.into()));
            }
            match read(Incoming::Open, &obj(&fields)) {
                Some(Message::Open { name, .. }) => name,
                _ => panic!("an open with bytes is read"),
            }
        };
        assert_eq!(named(None), "document");
        assert_eq!(named(Some("../../etc/passwd")), "passwd");
        assert_eq!(named(Some("a\u{0}b.txt")), "ab.txt");
        assert_eq!(named(Some(&"x".repeat(300))).chars().count(), crate::MAX_NAME_CHARS);
    }

    #[wasm_bindgen_test]
    fn the_size_is_the_data_s_own_not_the_hosts_claim() {
        // An ArrayBuffer of 64 MiB with a `size` field of 1: the bytes are viewed in place, not
        // copied, and the app sees their real length before it decides to read them.
        let ab = buffer(64 << 20);
        let Some(Message::Open { data, .. }) = read(Incoming::Open, &obj(&[("bytes", ab.clone().into()), ("size", 1.into())])) else {
            panic!("an open with bytes is read");
        };
        assert_eq!(data.size(), f64::from(64u32 << 20));
        let FileData::Bytes(view) = &data else { panic!("bytes stay bytes") };
        assert!(js_sys::Object::is(&view.buffer(), &ab), "a view of the host's buffer, not a copy");

        // A Blob of 5 bytes with a `size` field of a terabyte.
        let blob = web_sys::Blob::new_with_str_sequence(&js_sys::Array::of1(&"hello".into())).expect("a Blob");
        let Some(Message::Open { data, .. }) = read(Incoming::Open, &obj(&[("file", blob.into()), ("size", 1e12.into())])) else {
            panic!("an open with a Blob is read");
        };
        assert_eq!(data.size(), 5.0);
    }

    #[wasm_bindgen_test]
    fn a_saved_needs_a_string_id() {
        assert!(matches!(read(Incoming::Saved, &obj(&[("id", "s1".into())])), Some(Message::Saved { id }) if id == "s1"));
        for data in [obj(&[]), obj(&[("id", 3.into())]), JsValue::NULL, JsValue::from_str("s1")] {
            assert!(read(Incoming::Saved, &data).is_none());
        }
    }

    #[wasm_bindgen_test]
    fn a_host_message_and_a_handoff_done_read_whatever_fields_exist() {
        // Nothing required: a missing version is the contract's, `handoff` is true only for `true`.
        for data in [JsValue::NULL, obj(&[]), obj(&[("handoff", "yes".into())]), obj(&[("handoff", 1.into())])] {
            let Some(Message::Host { version, handoff }) = read(Incoming::Host, &data) else { panic!("a host message is read") };
            assert_eq!(version, crate::VERSION);
            assert!(!handoff);
        }
        assert!(matches!(read(Incoming::Host, &obj(&[("handoff", true.into())])), Some(Message::Host { handoff: true, .. })));
        assert!(matches!(
            read(Incoming::HandoffDone, &obj(&[("name", "a.pdf".into()), ("app", "pdfcraft".into())])),
            Some(Message::HandoffDone { name, app }) if name == "a.pdf" && app == "pdfcraft"
        ));
        assert!(matches!(read(Incoming::HandoffDone, &JsValue::NULL), Some(Message::HandoffDone { name, app }) if name.is_empty() && app.is_empty()));
    }
}
