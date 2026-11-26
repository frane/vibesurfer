//! Saved logins: cookies, local and session storage and IndexedDB,
//! captured from a page and restored into one.

use std::time::Duration;

use crate::engine::{EngineError, EngineResult};

use super::{IDB_LOAD_BODY_JS, IDB_SAVE_JS, STORAGE_LOAD_BODY_JS, STORAGE_SAVE_JS};

/// Captured storage state from `STORAGE_SAVE_JS`. Cookies live in a
/// separate host-side payload because `document.cookie` can't see
/// `HttpOnly`.
pub(crate) struct StorageSnapshot {
    pub url: String,
    pub origin: String,
    pub local_storage: std::collections::BTreeMap<String, String>,
    pub session_storage: std::collections::BTreeMap<String, String>,
    pub indexed_db: Vec<crate::backend::auth::IdbDatabase>,
    /// Records left behind because JSON cannot carry them (a Blob, a
    /// typed array). Reported rather than restored wrong.
    pub indexed_db_skipped: u32,
    /// The dump did not finish. The blob is missing databases it
    /// should have had, which is the one thing this must never be
    /// quiet about: a blob that looks complete and restores a logged
    /// out page is worse than one that says what it lacks.
    pub indexed_db_incomplete: bool,
}

pub(crate) fn run_save_storage_only<F, T>(eval: F, tick: T) -> EngineResult<StorageSnapshot>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    let json = eval(STORAGE_SAVE_JS, STORAGE_EVAL_BUDGET)?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value = serde_json::from_str(&unwrapped)
        .map_err(|e| EngineError::Other(format!("save_storage parse: {e}")))?;
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let origin = v
        .get("origin")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let local_storage = parse_storage_map(v.get("localStorage"));
    let session_storage = parse_storage_map(v.get("sessionStorage"));
    let (indexed_db, indexed_db_skipped, indexed_db_incomplete) = run_save_indexed_db(&eval, tick)?;
    Ok(StorageSnapshot {
        url,
        origin,
        local_storage,
        session_storage,
        indexed_db,
        indexed_db_skipped,
        indexed_db_incomplete,
    })
}

/// How long an async storage script is given to reach a verdict.
///
/// A deadline rather than a poll count, because what the loop is
/// really waiting on is the database, and a fixed number of turns
/// means something different on an idle machine than on one running
/// the whole cell suite at once.
///
/// The `tick` between polls is the mechanism that makes any of this
/// work. An eval only pumps the run loop until its own completion
/// handler fires, so a bare poll loop inside one engine job spins
/// through every attempt in microseconds while the database's
/// callbacks — which need run-loop turns on that same thread — never
/// run. Each backend hands in its own pump, exactly as `run_wait`
/// does.
const IDB_SETTLE_BUDGET: Duration = Duration::from_secs(15);

/// Budget for one storage-script hop. Deliberately not 5s: these calls
/// queue behind an engine that may be busy, and on a machine running
/// the whole cell suite at once a correct `vs auth save` was failing
/// `! TIMEOUT 5000ms eval` on scheduling delay alone. A hung page
/// still reports, just later.
const STORAGE_EVAL_BUDGET: Duration = Duration::from_secs(20);

/// Drive [`IDB_SAVE_JS`] to completion. Returns the databases and the
/// count of records JSON could not carry.
fn run_save_indexed_db<F, T>(
    eval: F,
    mut tick: T,
) -> EngineResult<(Vec<crate::backend::auth::IdbDatabase>, u32, bool)>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    let deadline = std::time::Instant::now() + IDB_SETTLE_BUDGET;
    let mut first = true;
    while std::time::Instant::now() < deadline {
        if !first {
            tick();
        }
        first = false;
        let raw = eval(IDB_SAVE_JS, STORAGE_EVAL_BUDGET)?;
        let unwrapped = serde_json::from_str::<String>(&raw).unwrap_or(raw);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&unwrapped) else {
            // A page with no IndexedDB at all, or an engine that gave
            // us nothing back: not a reason to fail the whole save.
            return Ok((Vec::new(), 0, false));
        };
        match v.get("state").and_then(serde_json::Value::as_str) {
            Some("pending") => {}
            // An origin that denies IndexedDB (private mode, a
            // policy) saves everything else rather than nothing.
            Some("error") | None => return Ok((Vec::new(), 0, false)),
            Some(_) => {
                let dbs = v
                    .get("dbs")
                    .map(|d| serde_json::from_value(d.clone()).unwrap_or_default())
                    .unwrap_or_default();
                let skipped = v
                    .get("skipped")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0);
                return Ok((dbs, skipped, false));
            }
        }
    }
    // Still working after every poll. Save the rest of the blob —
    // cookies and web storage are worth having — but say that the
    // databases are missing.
    Ok((Vec::new(), 0, true))
}

/// Drive [`IDB_LOAD_BODY_JS`] to completion. A restore that never
/// finishes is reported, not swallowed: the caller is about to act as
/// though it were logged in.
fn run_load_indexed_db<F, T>(
    eval: F,
    dbs: &[crate::backend::auth::IdbDatabase],
    mut tick: T,
) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    if dbs.is_empty() {
        return Ok(());
    }
    // The token makes a second `auth load` on the same page start a
    // fresh restore instead of reading the previous one's state.
    let token = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let payload = serde_json::json!({ "token": token, "dbs": dbs });
    let payload_lit = serde_json::to_string(&payload.to_string())
        .map_err(|e| EngineError::Other(format!("idb payload encode: {e}")))?;
    let body = IDB_LOAD_BODY_JS;
    // `return` on its own line ahead of the body would be a bare
    // `return;` — the body opens with a comment, and automatic
    // semicolon insertion ends the statement at the newline. Bind it
    // to a variable, where ASI cannot cut in.
    let js = format!(
        "(function() {{ var payload = JSON.parse({payload_lit}); var out = {body}; return out; }})()"
    );
    let deadline = std::time::Instant::now() + IDB_SETTLE_BUDGET;
    let mut first = true;
    while std::time::Instant::now() < deadline {
        if !first {
            tick();
        }
        first = false;
        let raw = eval(&js, STORAGE_EVAL_BUDGET)?;
        let unwrapped = serde_json::from_str::<String>(&raw).unwrap_or(raw);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&unwrapped) else {
            return Err(EngineError::Other("load_indexed_db: bad reply".into()));
        };
        match v.get("state").and_then(serde_json::Value::as_str) {
            Some("pending") => {}
            Some("done") => {
                // A restore that "succeeded" into an empty database is
                // the one outcome worth shouting about; the script's
                // own breadcrumbs are the only way to see which turn it
                // took, so they ride along on the success path too.
                if std::env::var("VS_IDB_TRACE").is_ok() {
                    if let Some(t) = v.get("trace").and_then(serde_json::Value::as_str) {
                        eprintln!("[vs] idb load trace: {t}");
                    }
                }
                return Ok(());
            }
            _ => {
                let msg = v
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                return Err(EngineError::Other(format!("load_indexed_db: {msg}")));
            }
        }
    }
    Err(EngineError::Timeout {
        budget: IDB_SETTLE_BUDGET,
        primitive: "auth_load:indexeddb",
        detail: "",
    })
}

fn parse_storage_map(v: Option<&serde_json::Value>) -> std::collections::BTreeMap<String, String> {
    let Some(obj) = v.and_then(serde_json::Value::as_object) else {
        return std::collections::BTreeMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

pub(crate) fn run_load_storage_only<F, T>(
    eval: F,
    local: &std::collections::BTreeMap<String, String>,
    session: &std::collections::BTreeMap<String, String>,
    indexed_db: &[crate::backend::auth::IdbDatabase],
    tick: T,
) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    let payload = serde_json::json!({
        "localStorage": local,
        "sessionStorage": session,
    });
    let payload_lit = serde_json::to_string(&payload.to_string())
        .map_err(|e| EngineError::Other(format!("storage payload encode: {e}")))?;
    let body = STORAGE_LOAD_BODY_JS;
    let js = format!("(function() {{ const payload = JSON.parse({payload_lit}); {body} }})()");
    let result = eval(&js, STORAGE_EVAL_BUDGET)?;
    let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
    if unwrapped != "ok" {
        return Err(EngineError::Other(format!(
            "load_storage: unexpected: {unwrapped}"
        )));
    }
    run_load_indexed_db(eval, indexed_db, tick)
}
