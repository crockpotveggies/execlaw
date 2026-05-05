//! identity-local-address-book — execlaw's in-tree reference
//! identity provider plugin (§2.14).
//!
//! Reads a JSON contact list from `EXECLAW_CONTACTS_PATH`
//! (default `~/.execlaw/contacts.json`) at startup. Each contact is:
//!
//! ```json
//! {
//!   "principal_id": "pri_aunt_marge",
//!   "display_name": "Aunt Marge",
//!   "trust_hint": "Family",
//!   "confidence": 0.99,
//!   "tags": ["family"],
//!   "identifiers": [
//!     {"transport": "phone", "handle": "+15551234567"},
//!     {"transport": "email", "handle": "marge@example.com"}
//!   ]
//! }
//! ```
//!
//! Responds to JSON-RPC `identity.resolve({transport, handle})` with
//! `{"match": {...IdentityMatch}}` if the (transport, handle) pair
//! matches any contact's identifier list, else `{"match": null}`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContactIdentifier {
    transport: String,
    handle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Contact {
    principal_id: String,
    display_name: String,
    trust_hint: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
    #[serde(default)]
    tags: Vec<String>,
    identifiers: Vec<ContactIdentifier>,
}

fn default_confidence() -> f64 {
    0.95
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcOk {
    id: u64,
    result: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcErr {
    id: u64,
    error: RpcErrBody,
}

#[derive(Debug, Serialize)]
struct RpcErrBody {
    code: i32,
    message: String,
}

#[derive(Debug, Serialize)]
struct IdentityMatch {
    stable_principal_id: String,
    confidence: f64,
    trust_hint: String,
    display_name: Option<String>,
    tags: Vec<String>,
    resolved_by: String,
}

/// Lookup table: (transport, handle) → contact.
struct ContactBook {
    by_id: HashMap<(String, String), Contact>,
}

impl ContactBook {
    fn from_path(path: &std::path::Path) -> Self {
        let by_id = match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Vec<Contact>>(&s) {
                Ok(contacts) => {
                    let mut map = HashMap::new();
                    for c in contacts {
                        for ident in &c.identifiers {
                            map.insert((ident.transport.clone(), ident.handle.clone()), c.clone());
                        }
                    }
                    map
                }
                Err(e) => {
                    eprintln!("identity-local-address-book: bad contacts JSON: {e}");
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(), // missing file = empty book
        };
        Self { by_id }
    }

    fn lookup(&self, transport: &str, handle: &str) -> Option<&Contact> {
        self.by_id.get(&(transport.to_owned(), handle.to_owned()))
    }
}

fn handle(book: &ContactBook, req: &RpcRequest) -> Result<serde_json::Value, (i32, String)> {
    match req.method.as_str() {
        "identity.resolve" => {
            let transport = req
                .params
                .get("transport")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing 'transport' param".to_owned()))?;
            let handle_str = req
                .params
                .get("handle")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing 'handle' param".to_owned()))?;
            match book.lookup(transport, handle_str) {
                Some(c) => {
                    let m = IdentityMatch {
                        stable_principal_id: c.principal_id.clone(),
                        confidence: c.confidence,
                        trust_hint: c.trust_hint.clone(),
                        display_name: Some(c.display_name.clone()),
                        tags: c.tags.clone(),
                        resolved_by: "identity-local-address-book".to_owned(),
                    };
                    Ok(serde_json::json!({"match": m}))
                }
                None => Ok(serde_json::json!({"match": null})),
            }
        }
        "shutdown" => std::process::exit(0),
        other => Err((-32601, format!("unknown method '{other}'"))),
    }
}

fn contacts_path() -> PathBuf {
    if let Ok(p) = std::env::var("EXECLAW_CONTACTS_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".execlaw").join("contacts.json")
}

fn main() {
    let book = ContactBook::from_path(&contacts_path());

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("identity-local-address-book: unparseable: {e}: {trimmed}");
                continue;
            }
        };
        let resp = match handle(&book, &req) {
            Ok(result) => serde_json::to_string(&RpcOk { id: req.id, result }).unwrap_or_default(),
            Err((code, message)) => serde_json::to_string(&RpcErr {
                id: req.id,
                error: RpcErrBody { code, message },
            })
            .unwrap_or_default(),
        };
        if writeln!(stdout, "{resp}").is_err() {
            break;
        }
        if stdout.flush().is_err() {
            break;
        }
    }
}
