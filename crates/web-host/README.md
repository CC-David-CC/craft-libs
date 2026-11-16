# craft-web-host

The host messages between a Craft app's web build and the page that hosts it, as one contract and one bridge for every app (storytold/craft-libs#358). A page frames the app with `?host=parent`, or runs it directly with `data-host="page"` on its canvas; either way the same messages pass files in, saves out, and unsaved-changes state, so a page can host any of the apps the same way.

The crate has two parts:

- **The contract** (all targets): the message kinds and their `<app>:<kind>` types, the contract version, how the hosting mode is read from the URL and the canvas, and the clean-up of file names a host passes in. Pure, tested natively.
- **The bridge** (`web`, wasm32 only): `Host::connect(app, canvas)` finds the host, `listen` hears its messages, and `post_ready`, `post_save`, `post_dirty`, `post_failed` and `post_handoff` answer it. Only the same-origin parent window is heard and answered; with a parent on another origin the app runs as if unframed.

## Messages

| Kind | Direction | Fields | Meaning |
|---|---|---|---|
| `ready` | app → host | `version`, `appVersion?`, `file?` | the app can take a document; `file: true` means it reads a `File`/`Blob` in slices |
| `host` | host → app | `version`, `handoff?` | what the host supports; `handoff: true` means it routes hand-offs |
| `open` | host → app | `name`, and `bytes` or `file` + `size` | open a document as File › Open would |
| `save` | app → host | `name`, `bytes`, `id?`, `saveAs?` | Save, Save As or Export: the host stores the file |
| `saved` | host → app | `id` | the captured save is durable; the app reconciles that revision without clearing newer edits |
| `save-failed` | host → app | `id`, `outcome`, `error` | explicit `failed`, `cancelled`, `conflict` or `unknown` outcome |
| `dirty` | app → host | `dirty` | unsaved changes appeared or went away |
| `failed` | app → host | `error`, `id?` | a document or a save could not be taken |
| `handoff` | app → host | `name`, and `bytes` or `file` + `size` | this file belongs to another app |
| `handoff-done` | host → app | `name`, `app` | the host gave it to the app that edits it |

With `?host=parent` a message is an object with `type` (`pdfcraft:open`) and the fields, sent with same-origin `postMessage`; `bytes` go as a transferred `ArrayBuffer`. With `data-host="page"` it is a `CustomEvent` of that type on the canvas, with the fields in `detail`.

Compatibility with the bridges shipped before this crate:

- Fields marked `?` are optional. A host answers `saved` only for a `save` that carries an `id`; posting without an ID provides no durability confirmation.
- A host that never sends `host` gets the behaviour of the earlier bridges.
- `size` beside `file` is for hosts and older bridges; this crate takes a `Blob`'s length from the `Blob` itself.
- A page host may dispatch an event while the app's handler runs (a `saved` answering the `save` it just posted). The bridge holds it and calls the handler again once the running call returns, so messages arrive one at a time and in order.
- `version` is `"1"`. The earlier bridges put the app's own release in it (`"0.14.0"`), so a host treats a `version` it doesn't know as `"1"`. The crate sends the release as `appVersion`.

## Use in an app's web shell

```rust
#[cfg(target_arch = "wasm32")]
if let Some(host) = craft_web_host::web::Host::connect("pdfcraft", &canvas) {
    host.listen(move |message| match message {
        craft_web_host::web::Message::Open { name, data } => {
            // Check data.size() against the app's limit before copying it in.
        }
        craft_web_host::web::Message::Saved { id } => { /* clear dirty for that save */ }
        _ => {}
    });
    let _ = host.post_ready(env!("CARGO_PKG_VERSION"), false);
    // Saves: host.post_save(name, &bytes, None, save_as); unsaved state: host.post_dirty(dirty).
}
```

The page command API is [`craft-web-api`](../web-api/README.md) (storytold/craft-libs#413): `window.<app>.request` and the command functions. It sits beside these messages. Opening, saving, the document name and the dirty flag stay here.

## Identified saves

`Host::post_identified_save(name, bytes, save_as, &SaveContext { ... })` adds
`documentId`, `revision`, `operation` and `documentSave` to the existing save envelope.
Capture these values when serializing. The editor revision is not a server revision;
the host must bind the request to its authorized instance, session and account resource.
The filename and identity strings grant no access. `documentSave` is false for exports,
even when the exported filename resembles the source filename.

A successful post only accepts dispatch. Match `Saved` or `SaveFailed` by request ID,
then reconcile the captured document, preserving subsequent edits. Failure replies need
an explicit outcome and a bounded diagnostic free of credentials. `Unknown` means the
mutation may have committed: retain its identity and reconcile rather than blindly retry.
Missing or unrecognized outcomes are dropped, never inferred as success or known failure.
The original `post_save` API remains available for existing consumers.

Request, document and editor-revision identities are nonempty, contain no control
characters and are limited to 128 Unicode scalars. Operations are 1–64 ASCII
letters, digits, periods, underscores or hyphens. Failure diagnostics are limited
to 1024 Unicode scalars. Old save envelopes remain unchanged, but Rust consumers
matching `Incoming` or `Message` exhaustively must handle the new failure variant
when updating their dependency.

## Correlated binary opens

A bridge with completed/error native adoption wiring calls
`post_ready_with_open_results`, advertising `ready.openResult: true`. The host
may then include a bounded `id` with the existing binary `open` message. The
bridge receives `Message::IdentifiedOpen`; malformed explicit IDs are dropped,
never converted into an untracked open. Old opens without IDs remain unchanged.

After actual successful adoption, call `post_opened(&OpenResult { ... })` with
the captured editor document ID/revision and `source_retained`. This flag is
false for repaired/converted/unnamed copies that must not overwrite the original
account resource. Errors use the existing app-to-host `failed {id,error}`.
Receipt/dispatch is not successful adoption. Bind the request ID to the host's
captured instance/session/account path/server revision; editor identities are
not authorization grants. Do not blindly replay an unconfirmed open. Existing
Rust exhaustive matches must deliberately handle the new enum variants.
