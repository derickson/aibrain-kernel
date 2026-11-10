//! Editing notes from the browser — the one place this service writes into a
//! vault.
//!
//! Everything else here reads the vaults. This module adds three writes —
//! replace a note's text, create an empty note at a vault's root — and one
//! read that goes with them (the raw file, frontmatter and all). The rules
//! every write follows:
//!
//! - **The path comes from the database, never the caller.** A save names a
//!   note id; the file is `brain.root` + `note.rel_path` from that row. A
//!   create names a brain id and a *title*, which is checked to be a single
//!   plain file name before `.md` is appended.
//! - **The target must be inside the vault after symlinks are resolved.** The
//!   root and the target are both canonicalised and the one must be a prefix
//!   of the other, so a symlink inside a vault that points out of it is
//!   refused rather than followed.
//! - **Only markdown.** `.md` / `.markdown`, on the path and on whatever a
//!   symlink resolves to.
//! - **Only enabled brains.** The brain must be enabled in the database *and*
//!   present in the current config with the same root.
//! - **Writes are atomic and conditional.** The caller sends the hash of the
//!   text it started from; if the file no longer has it, nothing is written
//!   and the caller gets the current text back. Otherwise a temp file in the
//!   same directory is written, given the original's permissions, synced and
//!   renamed over the original.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, Ctx};
use crate::config::BrainSpec;
use crate::db;

type Shared = Arc<Ctx>;
type ApiResult<T> = std::result::Result<T, ApiError>;

/// The largest note the editor will load or save.
pub const MAX_NOTE_BYTES: usize = 2 * 1024 * 1024;

/// The request body cap for a save. The text travels JSON-escaped — Python's
/// `json.dumps` writes every non-ASCII character as `\uXXXX`, up to three times
/// the UTF-8 size — so the body limit is well above the text limit, which is
/// checked separately once the body is decoded.
const MAX_SAVE_BODY: usize = 8 * 1024 * 1024;

/// Longest title a create accepts, in characters.
const MAX_TITLE_CHARS: usize = 200;

/// One save or create at a time. The hash check and the rename are two steps;
/// serialising our own writes means two tabs saving the same note cannot both
/// pass the check. (An outside editor can still race us — that is what the
/// conflict check catches on the next save.)
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/note/:id/source",
            get(read_source).post(save_source).layer(DefaultBodyLimit::max(MAX_SAVE_BODY)),
        )
        .route("/notes/create", post(create_note))
        .route("/resolve", get(resolve))
}

// ── refusals ───────────────────────────────────────────────────────────────

/// Why a path or title was refused. Each carries text written for the person
/// using the editor; none carries an absolute path.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    BadRequest(&'static str),
    Forbidden(&'static str),
    NotFound(&'static str),
    Exists,
}

impl Refusal {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            Refusal::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Refusal::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            Refusal::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Refusal::Exists => (StatusCode::CONFLICT, "a note with that name already exists"),
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

fn error(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

// ── pure checks (unit-tested below) ────────────────────────────────────────

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_markdown_name(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("md") | Some("markdown")
    )
}

/// A title a new note may be created under, trimmed, or why not.
///
/// Refused rather than sanitised, for the same reason `link_name` in the
/// Python server refuses: quietly turning `../x` into `x` creates a note
/// nobody asked for.
pub fn validate_title(raw: &str) -> Result<String, Refusal> {
    let title = raw.trim();
    if title.is_empty() {
        return Err(Refusal::BadRequest("a title is required"));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(Refusal::BadRequest("that title is too long"));
    }
    if title.contains('/') || title.contains('\\') {
        return Err(Refusal::BadRequest("a title cannot contain / or \\"));
    }
    if title.contains("..") {
        return Err(Refusal::BadRequest("a title cannot contain .."));
    }
    if title.starts_with('.') {
        return Err(Refusal::BadRequest("a title cannot start with ."));
    }
    if title.chars().any(char::is_control) {
        return Err(Refusal::BadRequest("a title cannot contain control characters"));
    }
    Ok(title.to_string())
}

/// Is a relative path from the database a plain, downward, markdown path?
///
/// It came from our own scan so it should be, but the check is cheap and the
/// row is the only thing between a request and a write.
fn check_rel_path(rel: &str) -> Result<(), Refusal> {
    if rel.is_empty() || rel.contains('\0') {
        return Err(Refusal::Forbidden("that note's path is not usable"));
    }
    let path = Path::new(rel);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(Refusal::Forbidden("that note's path leaves its vault")),
        }
    }
    if !is_markdown_name(path) {
        return Err(Refusal::Forbidden("only markdown notes can be edited"));
    }
    Ok(())
}

/// The real path of an existing note, confined to its vault.
///
/// Both ends are canonicalised, which resolves every symlink on the way: a
/// link inside the vault that points out of it fails the prefix check, and a
/// link to something that is not markdown fails the extension check on the
/// resolved name. The returned path is the real file, so an atomic replace
/// renames over the file itself rather than over a symlink to it.
pub fn confine_existing(root: &Path, rel: &str) -> Result<PathBuf, Refusal> {
    check_rel_path(rel)?;
    let root = std::fs::canonicalize(root)
        .map_err(|_| Refusal::Forbidden("that note's vault is not reachable"))?;
    let real = match std::fs::canonicalize(root.join(rel)) {
        Ok(real) => real,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Refusal::NotFound("that note is no longer on disk"))
        }
        Err(_) => return Err(Refusal::Forbidden("that note is not readable")),
    };
    if !real.starts_with(&root) || real == root {
        return Err(Refusal::Forbidden("that note resolves outside its vault"));
    }
    if !is_markdown_name(&real) {
        return Err(Refusal::Forbidden("only markdown notes can be edited"));
    }
    if !real.is_file() {
        return Err(Refusal::Forbidden("that note is not a regular file"));
    }
    Ok(real)
}

/// Where a new note called `title` would go at the root of `root`, confined.
///
/// The title is validated here too, so no caller can skip it. The file must
/// not exist in any form — not even as a dangling symlink, which a later
/// `create_new` would also refuse, but this way the caller gets a 409 rather
/// than an I/O error.
pub fn confine_new(root: &Path, title: &str) -> Result<PathBuf, Refusal> {
    let title = validate_title(title)?;
    let root = std::fs::canonicalize(root)
        .map_err(|_| Refusal::Forbidden("that vault is not reachable"))?;
    let target = root.join(format!("{title}.md"));
    // A title is one component with no separators, so its parent is the root;
    // checked anyway in case a future rule loosens `validate_title`.
    if target.parent() != Some(root.as_path()) {
        return Err(Refusal::BadRequest("a title must be a single file name"));
    }
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(Refusal::Exists);
    }
    Ok(target)
}

/// A folder a new note may be created in: vault-relative, `/`-separated, each
/// component held to the same rules as a title, so it cannot climb out of the
/// vault or hide itself. Empty means the vault root.
pub fn validate_folder(raw: &str) -> Result<Vec<String>, Refusal> {
    let raw = raw.trim().trim_matches('/');
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        let part = validate_title(part)
            .map_err(|_| Refusal::BadRequest("that folder is not a plain path inside the vault"))?;
        parts.push(part);
    }
    if parts.len() > 8 {
        return Err(Refusal::BadRequest("that folder is nested too deeply"));
    }
    Ok(parts)
}

/// [`confine_new`], but inside `folder` (created if missing). The folder is
/// canonicalised after it exists, so a symlinked folder pointing out of the
/// vault is refused rather than written through.
pub fn confine_new_in(root: &Path, folder: &[String], title: &str) -> Result<PathBuf, Refusal> {
    if folder.is_empty() {
        return confine_new(root, title);
    }
    let root = std::fs::canonicalize(root)
        .map_err(|_| Refusal::Forbidden("that vault is not reachable"))?;
    let dir = folder.iter().fold(root.clone(), |acc, part| acc.join(part));
    std::fs::create_dir_all(&dir).map_err(|_| Refusal::Forbidden("that folder could not be created"))?;
    let real = std::fs::canonicalize(&dir)
        .map_err(|_| Refusal::Forbidden("that folder is not reachable"))?;
    if !real.starts_with(&root) || real == root {
        return Err(Refusal::Forbidden("that folder resolves outside its vault"));
    }
    confine_new(&real, title)
}

/// A save in two steps, so the note itself is updated in place.
///
/// Replacing the file by rename (the usual atomic write) gives the note a new
/// inode on every save: its creation date resets (Obsidian's "created" sort,
/// Dataview's `file.ctime`), and Finder tags, other extended attributes, ACLs
/// and hard links are lost. Obsidian itself writes notes in place, so we do
/// too — and keep what atomicity bought us with a recovery copy instead:
///
/// 1. [`write_recovery`] puts the new text in a hidden `.aibrain-<nonce>.tmp`
///    beside the note and `fsync`s it. This is the slow part, and it runs
///    before the hash check, outside the window where Obsidian could slip a
///    write in between our check and our write.
/// 2. [`overwrite_in_place`] writes the bytes over the note (same inode),
///    trims it to length and `fsync`s. Only then is the recovery copy removed.
///    If anything fails, or the process dies, the copy stays: the user's text
///    is on disk either way.
///
/// The copy's name is short (a long note name plus a suffix could exceed the
/// 255-byte name limit), hidden, and not markdown, so neither the scanner
/// (skips dot-files) nor the watcher (markdown only) ever ingests it.
pub fn write_recovery(real: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let dir = real
        .parent()
        .ok_or_else(|| std::io::Error::other("a note always has a parent directory"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".aibrain-{}-{nonce:x}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    match result {
        Ok(()) => Ok(tmp),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Step 2 of a save: the bytes over the existing file, keeping its inode and
/// with it the creation date, permissions, extended attributes and links.
/// The recovery copy is removed only once the note is safely written.
pub fn overwrite_in_place(real: &Path, bytes: &[u8], recovery: &Path) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let result = (|| {
        // Not `truncate(true)`: writing first and trimming after means a
        // reader never sees an empty note, only (briefly) the new text.
        let mut file = std::fs::OpenOptions::new().write(true).open(real)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(bytes)?;
        file.set_len(bytes.len() as u64)?;
        file.sync_all()
    })();
    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(recovery);
            Ok(())
        }
        Err(err) => {
            tracing::error!(
                "writing {} failed ({err}); the new text is kept in {}",
                real.display(),
                recovery.display()
            );
            Err(err)
        }
    }
}

fn mtime_of(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Strip a wikilink target down to the part that names a note, the way
/// `vault::link_targets` does: `Target|alias` and `Target#heading` both
/// become `Target`.
pub fn wikilink_target(raw: &str) -> &str {
    let t = raw.trim();
    let t = t.strip_prefix("[[").unwrap_or(t);
    let t = t.strip_suffix("]]").unwrap_or(t);
    let t = t.split('|').next().unwrap_or("");
    let t = t.split('#').next().unwrap_or("").trim();
    t.strip_suffix(".md").unwrap_or(t).trim()
}

// ── the brain gate ─────────────────────────────────────────────────────────

/// The enabled brain whose root the database row names, or a refusal.
///
/// Enabled in the database is not enough on its own: a brain switched off in
/// the config keeps its row (and notes) until the next reindex retires it. So
/// the brain must also be in the config — which only lists enabled brains
/// whose directory exists — and the two must agree on where it lives.
fn enabled_brain(ctx: &Ctx, brain_id: &str, db_root: &str, db_enabled: bool) -> Result<BrainSpec, Response> {
    if !db_enabled {
        return Err(error(StatusCode::FORBIDDEN, "that brain is disabled"));
    }
    let cfg = ctx.config().map_err(|e| ApiError::from(e).into_response())?;
    let Some(spec) = cfg.brains.into_iter().find(|b| b.id == brain_id) else {
        return Err(error(StatusCode::FORBIDDEN, "that brain is not enabled in the config"));
    };
    let same = match (std::fs::canonicalize(&spec.root), std::fs::canonicalize(db_root)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err(error(
            StatusCode::CONFLICT,
            "that brain has moved since it was indexed — reindex first",
        ));
    }
    Ok(spec)
}

/// Note row → (brain, real path), with every check above applied.
async fn locate_note(ctx: &Ctx, id: i64) -> ApiResult<Result<(db::NoteLocation, BrainSpec, PathBuf), Response>> {
    let Some(loc) = db::note_location(&ctx.pool, id).await? else {
        return Ok(Err(error(StatusCode::NOT_FOUND, "no such note")));
    };
    let spec = match enabled_brain(ctx, &loc.brain_id, &loc.root, loc.brain_enabled) {
        Ok(spec) => spec,
        Err(resp) => return Ok(Err(resp)),
    };
    match confine_existing(Path::new(&loc.root), &loc.rel_path) {
        Ok(real) => Ok(Ok((loc, spec, real))),
        Err(refusal) => Ok(Err(refusal.into_response())),
    }
}

/// Re-ingest one note now, so the index (and `/events`) catch up without
/// waiting for — or depending on — the watcher.
async fn reingest(ctx: &Arc<Ctx>, spec: &BrainSpec, rel: &str) -> anyhow::Result<()> {
    crate::watch::apply_located(ctx, &[(spec, rel.to_string())]).await
}

// ── handlers ───────────────────────────────────────────────────────────────

async fn read_source(State(ctx): State<Shared>, UrlPath(id): UrlPath<i64>) -> ApiResult<Response> {
    let (loc, _spec, real) = match locate_note(&ctx, id).await? {
        Ok(found) => found,
        Err(resp) => return Ok(resp),
    };
    let bytes = tokio::fs::read(&real).await?;
    if bytes.len() > MAX_NOTE_BYTES {
        return Ok(error(StatusCode::PAYLOAD_TOO_LARGE, "that note is too large to edit here"));
    }
    let hash = sha256_hex(&bytes);
    let Ok(text) = String::from_utf8(bytes) else {
        // Saving lossy text back would corrupt the file, so it is not offered.
        return Ok(error(StatusCode::UNPROCESSABLE_ENTITY, "that note is not valid UTF-8"));
    };
    Ok(Json(json!({
        "nid": loc.nid,
        "brainId": loc.brain_id,
        "relPath": loc.rel_path,
        "name": loc.title,
        "text": text,
        "hash": hash,
        "mtime": mtime_of(&real),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SaveBody {
    text: String,
    #[serde(rename = "baseHash")]
    base_hash: String,
}

async fn save_source(
    State(ctx): State<Shared>,
    UrlPath(id): UrlPath<i64>,
    Json(body): Json<SaveBody>,
) -> ApiResult<Response> {
    if body.text.len() > MAX_NOTE_BYTES {
        return Ok(error(StatusCode::PAYLOAD_TOO_LARGE, "that note is too large to save here"));
    }
    let (loc, spec, real) = match locate_note(&ctx, id).await? {
        Ok(found) => found,
        Err(resp) => return Ok(resp),
    };

    let bytes = body.text.into_bytes();
    let hash = sha256_hex(&bytes);
    // The slow, fsync'd copy first, before the check — see `write_recovery`.
    let recovery = {
        let target = real.clone();
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || write_recovery(&target, &bytes)).await??
    };

    let (hash, mtime) = {
        let _guard = WRITE_LOCK.lock().await;
        let current = tokio::fs::read(&real).await?;
        let current_hash = sha256_hex(&current);
        if !current_hash.eq_ignore_ascii_case(body.base_hash.trim()) {
            let _ = tokio::fs::remove_file(&recovery).await;
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "conflict",
                    "text": String::from_utf8_lossy(&current),
                    "hash": current_hash,
                })),
            )
                .into_response());
        }
        let target = real.clone();
        tokio::task::spawn_blocking(move || overwrite_in_place(&target, &bytes, &recovery)).await??;
        (hash, mtime_of(&real))
    };

    if let Err(err) = reingest(&ctx, &spec, &loc.rel_path).await {
        // The file is saved; the watcher or the next reindex will catch the
        // index up. Failing the request would tell the user the save was lost.
        tracing::warn!("saved note {id} but could not re-ingest it: {err:#}");
    }
    Ok(Json(json!({ "hash": hash, "mtime": mtime })).into_response())
}

#[derive(Deserialize)]
struct CreateBody {
    brain: String,
    title: String,
    /// Vault-relative folder to create the note in; the root when absent.
    #[serde(default)]
    folder: String,
    /// What the new file starts with; empty when absent.
    #[serde(default)]
    text: String,
}

async fn create_note(State(ctx): State<Shared>, Json(body): Json<CreateBody>) -> ApiResult<Response> {
    let title = match validate_title(&body.title) {
        Ok(t) => t,
        Err(refusal) => return Ok(refusal.into_response()),
    };
    let Some((db_root, enabled)) = db::brain_root(&ctx.pool, &body.brain).await? else {
        return Ok(error(StatusCode::NOT_FOUND, "no such brain"));
    };
    let spec = match enabled_brain(&ctx, &body.brain, &db_root, enabled) {
        Ok(spec) => spec,
        Err(resp) => return Ok(resp),
    };
    let folder = match validate_folder(&body.folder) {
        Ok(f) => f,
        Err(refusal) => return Ok(refusal.into_response()),
    };
    if body.text.len() > MAX_NOTE_BYTES {
        return Ok(error(StatusCode::PAYLOAD_TOO_LARGE, "that note is too large to save here"));
    }
    let rel = if folder.is_empty() {
        format!("{title}.md")
    } else {
        format!("{}/{title}.md", folder.join("/"))
    };
    {
        let _guard = WRITE_LOCK.lock().await;
        let root = PathBuf::from(&db_root);
        let target = match tokio::task::spawn_blocking(move || confine_new_in(&root, &folder, &title)).await? {
            Ok(t) => t,
            Err(refusal) => return Ok(refusal.into_response()),
        };
        // `create_new` is O_EXCL: it refuses an existing file and will not
        // follow a symlink planted at the name between the check and here.
        let text = body.text.into_bytes();
        let made = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
            file.write_all(&text)?;
            file.sync_all()
        })
        .await?;
        match made {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(Refusal::Exists.into_response())
            }
            Err(err) => return Err(err.into()),
        }
    }
    reingest(&ctx, &spec, &rel).await?;
    match db::note_by_path(&ctx.pool, &spec.id, &rel).await? {
        Some(note) => Ok(Json(json!({ "nid": note.nid })).into_response()),
        // Created on disk but not indexed — an exclude rule matched it, say.
        None => Ok(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the note was created but could not be indexed",
        )),
    }
}

#[derive(Deserialize)]
struct ResolveParams {
    #[serde(default)]
    brain: String,
    title: String,
}

/// A wikilink target → note id, the way the reader resolves `[[Target]]`:
/// the same fold, the same query, the same preference for the brain the link
/// was written in, falling back to any brain.
async fn resolve(State(ctx): State<Shared>, Query(params): Query<ResolveParams>) -> ApiResult<Response> {
    if params.title.len() > 1024 || params.brain.len() > 256 {
        return Ok(error(StatusCode::BAD_REQUEST, "that link is too long"));
    }
    let target = wikilink_target(&params.title);
    if target.is_empty() {
        return Ok(error(StatusCode::BAD_REQUEST, "a title is required"));
    }
    let resolved = db::resolve_targets(&ctx.pool, &params.brain, &[target.to_string()]).await?;
    match resolved.get(&crate::vault::normalize(target)) {
        Some(nid) => Ok(Json(json!({ "nid": nid })).into_response()),
        None => Ok(error(StatusCode::NOT_FOUND, "no such note")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aibrain-edit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_is_lowercase_hex_of_the_bytes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b"").len(), 64);
    }

    #[test]
    fn plain_titles_are_accepted_and_trimmed() {
        assert_eq!(validate_title("  Meeting notes ").unwrap(), "Meeting notes");
        assert_eq!(validate_title("Café — 2026").unwrap(), "Café — 2026");
        assert_eq!(validate_title("v1.2 plan").unwrap(), "v1.2 plan");
    }

    #[test]
    fn dangerous_titles_are_refused_not_sanitised() {
        for bad in [
            "", "   ", "a/b", "a\\b", "..", "../etc", "x..y", ".hidden", "nul\0byte",
            "new\nline", "tab\there", "\u{7f}del", "/abs",
        ] {
            assert!(validate_title(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_title(&"x".repeat(MAX_TITLE_CHARS + 1)).is_err());
    }

    #[test]
    fn folders_are_plain_downward_paths() {
        assert_eq!(validate_folder("").unwrap(), Vec::<String>::new());
        assert_eq!(validate_folder("Absorbitron").unwrap(), vec!["Absorbitron"]);
        assert_eq!(validate_folder("/Tech/Products/").unwrap(), vec!["Tech", "Products"]);
        for bad in ["..", "a/../b", ".hidden", "a/.git", "a\\b", "a//b"] {
            assert!(validate_folder(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_new_note_in_a_folder_lands_there_and_never_leaves_the_vault() {
        let dir = tempdir("folder");
        let vault = dir.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let target = confine_new_in(&vault, &["Absorbitron".into()], "2026-11-10 1432 Idea").unwrap();
        assert_eq!(
            target,
            std::fs::canonicalize(&vault).unwrap().join("Absorbitron/2026-11-10 1432 Idea.md")
        );
        // A folder that is a symlink out of the vault is refused.
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, vault.join("Out")).unwrap();
        assert!(confine_new_in(&vault, &["Out".into()], "x").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wikilink_targets_lose_their_alias_and_heading() {
        assert_eq!(wikilink_target("Target"), "Target");
        assert_eq!(wikilink_target("Target#Heading"), "Target");
        assert_eq!(wikilink_target("Target|shown"), "Target");
        assert_eq!(wikilink_target("Target#H|shown"), "Target");
        assert_eq!(wikilink_target("[[Recipes/Sourdough]]"), "Recipes/Sourdough");
        assert_eq!(wikilink_target("Note.md"), "Note");
        assert_eq!(wikilink_target("#only-heading"), "");
    }

    #[test]
    fn database_paths_must_be_plain_downward_markdown() {
        assert!(check_rel_path("Note.md").is_ok());
        assert!(check_rel_path("a/b/Note.MARKDOWN").is_ok());
        assert!(check_rel_path("../Note.md").is_err());
        assert!(check_rel_path("a/../../Note.md").is_err());
        assert!(check_rel_path("/etc/passwd.md").is_err());
        assert!(check_rel_path("Note.txt").is_err());
        assert!(check_rel_path("").is_err());
        assert!(check_rel_path("a\0.md").is_err());
    }

    #[test]
    fn an_existing_note_resolves_inside_its_vault() {
        let dir = tempdir("inside");
        let vault = dir.join("vault");
        std::fs::create_dir_all(vault.join("Sub")).unwrap();
        std::fs::write(vault.join("Sub/Note.md"), "hi").unwrap();
        let real = confine_existing(&vault, "Sub/Note.md").unwrap();
        assert_eq!(real, std::fs::canonicalize(vault.join("Sub/Note.md")).unwrap());
        assert_eq!(
            confine_existing(&vault, "Missing.md"),
            Err(Refusal::NotFound("that note is no longer on disk"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_vault_is_refused() {
        let dir = tempdir("escape");
        let vault = dir.join("vault");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), "secret").unwrap();
        std::fs::write(outside.join("key.txt"), "key").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), vault.join("Link.md")).unwrap();
        std::os::unix::fs::symlink(&outside, vault.join("Dir")).unwrap();
        // A markdown-named link to a non-markdown file inside the vault.
        std::fs::write(vault.join("notes.txt"), "plain").unwrap();
        std::os::unix::fs::symlink(vault.join("notes.txt"), vault.join("Alias.md")).unwrap();

        assert!(matches!(confine_existing(&vault, "Link.md"), Err(Refusal::Forbidden(_))));
        assert!(matches!(confine_existing(&vault, "Dir/secret.md"), Err(Refusal::Forbidden(_))));
        assert!(matches!(confine_existing(&vault, "Alias.md"), Err(Refusal::Forbidden(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_within_the_vault_resolves_to_the_real_file() {
        let dir = tempdir("within");
        let vault = dir.join("vault");
        std::fs::create_dir_all(vault.join("Real")).unwrap();
        std::fs::write(vault.join("Real/Note.md"), "x").unwrap();
        std::os::unix::fs::symlink(vault.join("Real/Note.md"), vault.join("Alias.md")).unwrap();
        let real = confine_existing(&vault, "Alias.md").unwrap();
        assert_eq!(real, std::fs::canonicalize(vault.join("Real/Note.md")).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_vault_reached_through_a_symlink_still_confines() {
        // The real layout: obsidian_vaults/<name> is a link to the vault.
        let dir = tempdir("linked");
        let real_vault = dir.join("real");
        std::fs::create_dir_all(&real_vault).unwrap();
        std::fs::write(real_vault.join("Note.md"), "x").unwrap();
        std::os::unix::fs::symlink(&real_vault, dir.join("link")).unwrap();
        assert!(confine_existing(&dir.join("link"), "Note.md").is_ok());
        assert!(confine_new(&dir.join("link"), "Fresh").is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_new_note_never_overwrites_anything_even_a_dangling_link() {
        let dir = tempdir("new");
        let vault = dir.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("Taken.md"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("nowhere.md"), vault.join("Dangling.md")).unwrap();

        let fresh = confine_new(&vault, "Fresh idea").unwrap();
        assert_eq!(fresh, std::fs::canonicalize(&vault).unwrap().join("Fresh idea.md"));
        assert_eq!(confine_new(&vault, "Taken"), Err(Refusal::Exists));
        assert_eq!(confine_new(&vault, "Dangling"), Err(Refusal::Exists));
        assert!(matches!(confine_new(&vault, "../escape"), Err(Refusal::BadRequest(_))));
        assert!(matches!(confine_new(&vault, ".hidden"), Err(Refusal::BadRequest(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_save_updates_the_note_in_place_and_leaves_no_temp_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempdir("inplace");
        let note = dir.join("Note.md");
        std::fs::write(&note, "a much longer old text than the new one").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.join("Hard link.md");
        std::fs::hard_link(&note, &link).unwrap();
        let before = std::fs::metadata(&note).unwrap();

        let recovery = write_recovery(&note, "new text".as_bytes()).unwrap();
        assert_eq!(std::fs::read_to_string(&recovery).unwrap(), "new text");
        overwrite_in_place(&note, "new text".as_bytes(), &recovery).unwrap();

        assert_eq!(std::fs::read_to_string(&note).unwrap(), "new text", "trimmed to length");
        let after = std::fs::metadata(&note).unwrap();
        assert_eq!(after.ino(), before.ino(), "same inode");
        assert_eq!(after.permissions().mode() & 0o777, 0o640);
        if let (Ok(a), Ok(b)) = (before.created(), after.created()) {
            assert_eq!(a, b, "creation date kept");
        }
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new text", "hard link still shares it");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "Note.md" && n != "Hard link.md")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_save_keeps_extended_attributes() {
        let dir = tempdir("xattr");
        let note = dir.join("Tagged.md");
        std::fs::write(&note, "old").unwrap();
        let ok = std::process::Command::new("xattr")
            .args(["-w", "com.example.tag", "red"])
            .arg(&note)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // no xattr tool here; the inode check above covers it
        }
        let recovery = write_recovery(&note, b"new").unwrap();
        overwrite_in_place(&note, b"new", &recovery).unwrap();
        let out = std::process::Command::new("xattr")
            .args(["-p", "com.example.tag"])
            .arg(&note)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "red");
        std::fs::remove_dir_all(&dir).ok();
    }
}
