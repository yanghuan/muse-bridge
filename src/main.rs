//! muse-bridge: stateless QQ Mail IMAP/SMTP bridge.
//!
//! A single musl-static binary. No mailbox credentials are ever stored
//! server-side: every request carries its own `account: {user, auth_code}`,
//! so any number of QQ mailboxes can be served. The only server-side secret
//! is `BRIDGE_TOKEN`, which authorizes callers of this HTTP API
//! (important: without it this would be an open SMTP relay).
//!
//! Env:
//!   BRIDGE_TOKEN   required. Bearer token for API callers.
//!   LISTEN_ADDR    optional, default "127.0.0.1:8080".

mod imap;
mod smtp;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use imap::{ImapClient, MailError};
use serde::Deserialize;
use serde_json::{json, Value};
use smtp::SmtpClient;
use std::{env, net::SocketAddr, sync::Arc, time::Duration};

const QQ_IMAP_HOST: &str = "imap.qq.com";
const QQ_IMAP_PORT: u16 = 993;
const QQ_SMTP_HOST: &str = "smtp.qq.com";
const QQ_SMTP_PORT: u16 = 465; // implicit TLS
const OP_TIMEOUT_SECS: u64 = 90;

// ---------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Account {
    user: String,
    auth_code: String,
}

#[derive(Deserialize)]
struct ImapReq {
    account: Account,
    #[serde(default = "default_action")]
    action: String, // list | search | fetch
    #[serde(default = "default_mailbox")]
    mailbox: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    unseen: bool,
    #[serde(default)]
    criteria: String,
    uid: Option<u32>,
}

fn default_action() -> String {
    "list".to_string()
}
fn default_mailbox() -> String {
    "INBOX".to_string()
}
fn default_limit() -> usize {
    20
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ToField {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct SmtpReq {
    account: Account,
    from: Option<String>,
    to: ToField,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    html: String,
}

// ---------------------------------------------------------------------------
// Auth: constant-time-ish bearer check against BRIDGE_TOKEN.
// ---------------------------------------------------------------------------

fn check_auth(headers: &HeaderMap, token: &str) -> bool {
    let hdr = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Build the expected value without logging either side.
    let want = format!("Bearer {}", token);
    if hdr.len() != want.len() {
        return false;
    }
    hdr.bytes()
        .zip(want.bytes())
        .fold(0u8, |d, (a, b)| d | (a ^ b))
        == 0
}

fn unauthorized() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"ok": false, "error": "unauthorized"})),
    )
}

// ---------------------------------------------------------------------------
// Mail operations (one fresh connection per request = stateless)
// ---------------------------------------------------------------------------

async fn do_imap(req: ImapReq) -> Result<Value, MailError> {
    let mut client = ImapClient::open(
        QQ_IMAP_HOST,
        QQ_IMAP_PORT,
        &req.account.user,
        &req.account.auth_code,
    )
    .await?;
    let result = async {
        client.select(&req.mailbox).await?;
        match req.action.as_str() {
            "fetch" => {
                let uid = req
                    .uid
                    .ok_or_else(|| MailError("fetch requires uid".to_string()))?;
                let msg = client.fetch_full(uid, 256 * 1024).await?;
                Ok(json!({ "ok": true, "message": msg }))
            }
            "search" => {
                let criteria = if req.criteria.is_empty() {
                    "ALL"
                } else {
                    req.criteria.as_str()
                };
                let uids = client.uid_search(criteria).await?;
                let messages = client
                    .fetch_headers(&uids, req.limit.clamp(1, 100))
                    .await?;
                Ok(json!({ "ok": true, "total": uids.len(), "messages": messages }))
            }
            _ => {
                // list
                let uids = client
                    .uid_search(if req.unseen { "UNSEEN" } else { "ALL" })
                    .await?;
                let messages = client
                    .fetch_headers(&uids, req.limit.clamp(1, 100))
                    .await?;
                Ok(json!({ "ok": true, "total": uids.len(), "messages": messages }))
            }
        }
    }
    .await;
    client.close().await;
    result
}

async fn do_smtp(req: SmtpReq) -> Result<Value, MailError> {
    let to: Vec<String> = match req.to {
        ToField::One(s) => vec![s],
        ToField::Many(v) => v,
    };
    if to.is_empty() {
        return Err(MailError("smtp requires \"to\"".to_string()));
    }
    let from = req.from.unwrap_or_else(|| req.account.user.clone());
    let mut client = SmtpClient::open(
        QQ_SMTP_HOST,
        QQ_SMTP_PORT,
        &req.account.user,
        &req.account.auth_code,
    )
    .await?;
    let result = client
        .send(&from, &to, &req.subject, &req.text, &req.html)
        .await;
    client.close().await;
    result.map(|mid| json!({ "ok": true, "messageId": mid }))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_imap(
    State(token): State<Arc<String>>,
    headers: HeaderMap,
    Json(req): Json<ImapReq>,
) -> impl IntoResponse {
    if !check_auth(&headers, &token) {
        return unauthorized();
    }
    match tokio::time::timeout(Duration::from_secs(OP_TIMEOUT_SECS), do_imap(req)).await {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "ok": false, "error": "mail operation timed out" })),
        ),
    }
}

async fn handle_smtp(
    State(token): State<Arc<String>>,
    headers: HeaderMap,
    Json(req): Json<SmtpReq>,
) -> impl IntoResponse {
    if !check_auth(&headers, &token) {
        return unauthorized();
    }
    match tokio::time::timeout(Duration::from_secs(OP_TIMEOUT_SECS), do_smtp(req)).await {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "ok": false, "error": "mail operation timed out" })),
        ),
    }
}

#[tokio::main]
async fn main() {
    // Read the secret once at startup; never print it.
    let token = env::var("BRIDGE_TOKEN").expect("BRIDGE_TOKEN env var is required");
    let listen: SocketAddr = env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()
        .expect("LISTEN_ADDR is not a valid socket address");

    let app = Router::new()
        .route(
            "/",
            get(|| async {
                Json(json!({
                    "ok": true,
                    "service": "muse-bridge",
                    "endpoints": ["POST /imap", "POST /smtp"],
                }))
            }),
        )
        .route("/imap", post(handle_imap))
        .route("/smtp", post(handle_smtp))
        .with_state(Arc::new(token));

    eprintln!("muse-bridge listening on {}", listen);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("failed to bind LISTEN_ADDR");
    axum::serve(listener, app).await.expect("server error");
}
