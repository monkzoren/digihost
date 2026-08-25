//! GitHub webhook receiver.
//!
//! A push to an application's default branch redeploys it to wherever it is
//! already running. Nothing else is inferred: DigiHost will not invent a
//! target for an application that has never been deployed, because guessing
//! where to put code is worse than doing nothing.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use spacetimedb_sdk::Table;

use crate::module_bindings::{
    application_table::ApplicationTableAccess, deployment_table::DeploymentTableAccess,
    host_table::HostTableAccess, queue_deployment, DeployStatus, HostStatus,
};
use crate::AppState;

#[derive(Deserialize)]
pub struct PushEvent {
    /// "refs/heads/main"
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub after: String,
    pub repository: Repository,
    #[serde(default)]
    pub head_commit: Option<HeadCommit>,
}

#[derive(Deserialize)]
pub struct Repository {
    /// "owner/name"
    pub full_name: String,
}

#[derive(Deserialize)]
pub struct HeadCommit {
    pub message: String,
}

/// Constant-time comparison of the delivery signature. A short-circuiting
/// compare would leak the expected signature one byte at a time to anyone
/// willing to send enough deliveries.
fn signature_matches(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(provided) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(provided) = decode_hex(provided) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    if text.len() % 2 != 0 {
        return Err(());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

pub async fn receive(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let Some(config) = state.store.github().await else {
        return (StatusCode::NOT_FOUND, "no GitHub App configured").into_response();
    };
    let Some(secret) = config.webhook_secret.filter(|s| !s.is_empty()) else {
        // Refuse rather than accept unverified deliveries: an open endpoint
        // that queues deployments is worse than no endpoint.
        return (
            StatusCode::FORBIDDEN,
            "this instance has no webhook secret configured",
        )
            .into_response();
    };

    let provided = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !signature_matches(&secret, &body, provided) {
        return (StatusCode::UNAUTHORIZED, "signature mismatch").into_response();
    }

    let event = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    match event {
        // GitHub sends this when the webhook is first configured.
        "ping" => (StatusCode::OK, "pong").into_response(),
        "push" => match serde_json::from_slice::<PushEvent>(&body) {
            Ok(push) => handle_push(&state, push).await,
            Err(e) => {
                (StatusCode::BAD_REQUEST, format!("unreadable push event: {e}")).into_response()
            }
        },
        other => (StatusCode::OK, format!("ignoring {other} event")).into_response(),
    }
}

async fn handle_push(state: &AppState, push: PushEvent) -> Response {
    let branch = push.git_ref.strip_prefix("refs/heads/").unwrap_or_default();
    if branch.is_empty() {
        return (StatusCode::OK, "ignoring non-branch push").into_response();
    }

    let app = state
        .conn
        .db
        .application()
        .iter()
        .find(|a| a.repo.eq_ignore_ascii_case(&push.repository.full_name));
    let Some(app) = app else {
        return (
            StatusCode::OK,
            format!("no application registered for {}", push.repository.full_name),
        )
            .into_response();
    };

    if app.default_branch != branch {
        return (
            StatusCode::OK,
            format!(
                "ignoring push to {branch}; {} deploys from {}",
                app.name, app.default_branch
            ),
        )
            .into_response();
    }

    // Redeploy wherever this application already runs, reusing the strategy
    // each host last used. Never deployed → nothing to infer → nothing happens.
    let targets = targets_for(state, app.id);
    if targets.is_empty() {
        return (
            StatusCode::OK,
            format!("{} has never been deployed, so there is nowhere to redeploy it", app.name),
        )
            .into_response();
    }

    let message = push
        .head_commit
        .map(|c| c.message.lines().next().unwrap_or_default().to_string())
        .unwrap_or_else(|| format!("push to {branch}"));

    let mut queued = 0;
    for (host_id, strategy) in targets {
        match state.conn.reducers.queue_deployment(
            app.id,
            host_id,
            push.after.clone(),
            message.clone(),
            strategy,
        ) {
            Ok(()) => queued += 1,
            Err(e) => tracing::warn!("webhook could not queue for host {host_id}: {e}"),
        }
    }

    tracing::info!(app = %app.name, commit = %push.after, queued, "webhook queued deployments");
    (StatusCode::OK, format!("queued {queued} deployment(s)")).into_response()
}

/// Hosts this application has successfully deployed to, with the strategy each
/// most recently used. Offline and draining hosts are skipped — the control
/// plane would refuse them anyway, and a webhook should not fill the log with
/// refusals.
fn targets_for(state: &AppState, app_id: u64) -> Vec<(u64, String)> {
    let db = &state.conn.db;

    let mut deployments: Vec<_> = db
        .deployment()
        .iter()
        .filter(|d| d.app_id == app_id && d.status == DeployStatus::Succeeded)
        .collect();
    // Newest last, so the later insert wins and each host keeps its most
    // recent strategy.
    deployments.sort_by_key(|d| d.id);

    let mut by_host: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for d in deployments {
        by_host.insert(d.host_id, d.strategy.clone());
    }

    by_host
        .into_iter()
        .filter(|(host_id, _)| {
            db.host()
                .iter()
                .any(|h| h.id == *host_id && h.status == HostStatus::Online)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Signature from GitHub's own documented example, so this checks the real
    // algorithm rather than agreeing with itself.
    const SECRET: &str = "It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const EXPECTED: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn accepts_a_correct_signature() {
        assert!(signature_matches(SECRET, BODY, EXPECTED));
    }

    #[test]
    fn rejects_tampering() {
        assert!(!signature_matches(SECRET, b"Goodbye, World!", EXPECTED));
        assert!(!signature_matches("wrong secret", BODY, EXPECTED));
        assert!(!signature_matches(SECRET, BODY, "sha256=deadbeef"));
        // An unsigned delivery must never pass.
        assert!(!signature_matches(SECRET, BODY, ""));
        // A signature without the algorithm prefix is not a signature.
        assert!(!signature_matches(
            SECRET,
            BODY,
            "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        ));
    }

    #[test]
    fn decodes_hex_strictly() {
        assert_eq!(decode_hex("00ff10"), Ok(vec![0, 255, 16]));
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
