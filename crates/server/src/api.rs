//! HTTP surface: operator actions and agent-facing endpoints.
//!
//! Two audiences with two credentials. Operators present a session cookie;
//! agents present a bearer token issued at enrolment. Neither is a SpacetimeDB
//! credential — the server is the only client that speaks to the control
//! plane as an operator.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Json};
use serde::{Deserialize, Serialize};
use spacetimedb_sdk::__codegen::InternalError;
use spacetimedb_sdk::Table;
use tokio::sync::RwLock;

use crate::github::AppConfig;
use crate::module_bindings::{
    application_table::ApplicationTableAccess, claim_instance, create_enrollment_token,
    deployment_table::DeploymentTableAccess, host_table::HostTableAccess, queue_deployment,
    delete_application, delete_host, register_application, rollback_deployment,
    set_host_draining, DbConnection, DeployStatus, SourceKind,
};
use crate::store::{generate_token, parse_env};
use crate::AppState;

const SESSION_COOKIE: &str = "digihost_session";

/// Operator sessions are in memory: a restart signing everyone out is the
/// correct behaviour for a single-tenant box, and it avoids persisting
/// anything that grants access. Each session belongs to a named user.
#[derive(Clone, Default)]
pub struct Sessions(Arc<RwLock<std::collections::HashMap<String, String>>>);

impl Sessions {
    pub async fn create(&self, user: &str) -> anyhow::Result<String> {
        let token = generate_token()?;
        self.0.write().await.insert(token.clone(), user.to_string());
        Ok(token)
    }

    pub async fn user_for(&self, token: &str) -> Option<String> {
        self.0.read().await.get(token).cloned()
    }

    pub async fn revoke(&self, token: &str) {
        self.0.write().await.remove(token);
    }
}

/// Who an authenticated request is. Inserted by the middleware, so every
/// protected handler can know its caller.
#[derive(Clone)]
pub struct CurrentUser {
    pub name: String,
    pub admin: bool,
    /// True when the request authenticated with an API token rather than a
    /// browser session — those cannot manage accounts or change passwords.
    pub via_token: bool,
}

fn cookie_value(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Gate every operator surface, and record who the caller is.
///
/// Browsers authenticate with a session cookie; scripts may use an API token
/// as a bearer header on the action endpoints. Unauthenticated browsers land
/// on the login page; unauthenticated action calls get a 401.
pub async fn require_operator(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    // First run: nobody has claimed the instance, so setup is the only page.
    if !state.store.is_claimed().await {
        return Redirect::to("/setup").into_response();
    }

    // Session first. Admin is looked up fresh each request, so demoting or
    // deleting a user takes effect immediately, sessions included.
    let mut current: Option<CurrentUser> = None;
    if let Some(token) = cookie_value(&req, SESSION_COOKIE) {
        if let Some(name) = state.sessions.user_for(&token).await {
            if let Some(admin) = state.store.user_admin(&name).await {
                current = Some(CurrentUser { name, admin, via_token: false });
            }
        }
    }
    if current.is_none() {
        if let Some(token) = bearer(&req) {
            if let Some(name) = state.store.api_token_name(&token).await {
                current = Some(CurrentUser {
                    name: format!("token:{name}"),
                    admin: false,
                    via_token: true,
                });
            }
        }
    }

    match current {
        Some(user) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None if path.starts_with("/actions/") => {
            (StatusCode::UNAUTHORIZED, "sign in first").into_response()
        }
        None => Redirect::to("/login").into_response(),
    }
}

// ---------------------------------------------------------------- sessions

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    pub username: String,
    pub password: String,
}

pub async fn setup(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    if state.store.is_claimed().await {
        return Redirect::to("/login").into_response();
    }
    if let Err(resp) = password_acceptable(&form.password) {
        return resp;
    }
    let username = if form.username.trim().is_empty() {
        "admin".to_string()
    } else {
        form.username.clone()
    };
    // The first account claims the instance, so it is an administrator.
    let name = match state.store.add_user(&username, &form.password, true).await {
        Ok(n) => n,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    };
    issue_session(&state, &name).await
}

pub async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    if state.store.verify_login(&form.username, &form.password).await.is_none() {
        return Redirect::to("/login?error=1").into_response();
    }
    issue_session(&state, form.username.trim().to_lowercase().as_str()).await
}

fn password_acceptable(password: &str) -> Result<(), Response> {
    if password.chars().count() < 12 {
        return Err((
            StatusCode::BAD_REQUEST,
            "choose a password of at least 12 characters",
        )
            .into_response());
    }
    Ok(())
}

async fn issue_session(state: &AppState, user: &str) -> Response {
    match state.sessions.create(user).await {
        Ok(token) => (
            [(
                header::SET_COOKIE,
                // No Secure flag: DigiHost is commonly reached over plain HTTP
                // on a private network. Behind TLS this should gain one —
                // noted in PLAN.md.
                format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/"),
            )],
            Redirect::to("/"),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(token) = cookie_value(&req, SESSION_COOKIE) {
        state.sessions.revoke(&token).await;
    }
    (
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
        )],
        Redirect::to("/login"),
    )
        .into_response()
}

// ----------------------------------------------------------------- verdicts

/// Outcome of a reducer call, as reported by the control plane.
type Verdict = Result<Result<(), String>, InternalError>;

fn verdict_channel() -> (
    tokio::sync::oneshot::Sender<Verdict>,
    tokio::sync::oneshot::Receiver<Verdict>,
) {
    tokio::sync::oneshot::channel()
}

/// Wait for a reducer's actual result.
///
/// `conn.reducers.x()` only reports whether the *request* was sent; the
/// reducer may still refuse. Without waiting, the interface would cheerfully
/// report success for an action the control plane rejected.
async fn verdict(
    sent: Result<(), spacetimedb_sdk::Error>,
    rx: tokio::sync::oneshot::Receiver<Verdict>,
) -> Result<(), Response> {
    if let Err(e) = sent {
        return Err(
            (StatusCode::BAD_GATEWAY, format!("could not reach the control plane: {e}"))
                .into_response(),
        );
    }

    match tokio::time::timeout(Duration::from_secs(15), rx).await {
        Ok(Ok(Ok(Ok(())))) => Ok(()),
        // The reducer ran and refused: the case worth getting right.
        Ok(Ok(Ok(Err(reason)))) => Err((StatusCode::BAD_REQUEST, reason).into_response()),
        Ok(Ok(Err(e))) => Err((
            StatusCode::BAD_GATEWAY,
            format!("the control plane returned something unreadable: {e}"),
        )
            .into_response()),
        Ok(Err(_)) => Err((
            StatusCode::BAD_GATEWAY,
            "lost the control plane connection before the action completed",
        )
            .into_response()),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            "the control plane did not answer within 15s",
        )
            .into_response()),
    }
}

// ------------------------------------------------------------------ actions

#[derive(Deserialize)]
pub struct AddServerForm {
    pub region: String,
}

#[derive(Serialize)]
pub struct AddServerResult {
    pub code: String,
    pub command: String,
}

/// Mint an enrolment code and hand back the command to run on the new host.
pub async fn add_server(State(state): State<AppState>, Form(form): Form<AddServerForm>) -> Response {
    let region = match form.region.trim() {
        "" => "default".to_string(),
        r => r.to_string(),
    };

    let code = match generate_enrollment_code() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };

    // Record locally first: if the control plane accepted the code but we
    // forgot it, the agent could never redeem it for an HTTP token.
    if let Err(e) = state.store.remember_enrollment(&code, &region).await {
        return internal(e);
    }

    let (tx, rx) = verdict_channel();
    let sent = state
        .conn
        .reducers
        .create_enrollment_token_then(code.clone(), region, move |_ctx, outcome| {
            let _ = tx.send(outcome);
        });
    if let Err(response) = verdict(sent, rx).await {
        return response;
    }

    Json(AddServerResult {
        command: format!(
            "digihost-agent --server {} --enrollment-code {code}",
            state.public_url
        ),
        code,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct RegisterAppForm {
    pub name: String,
    pub repo: String,
    pub branch: String,
    /// "public" or "private"
    pub visibility: String,
    #[serde(default)]
    pub entrypoint: String,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub deploy_path: String,
    #[serde(default)]
    pub strategy: String,
}

pub async fn register_app(State(state): State<AppState>, Form(form): Form<RegisterAppForm>) -> Response {
    let kind = if form.visibility == "private" {
        SourceKind::GitHubApp
    } else {
        SourceKind::GitHubPublic
    };

    if kind == SourceKind::GitHubApp && !state.github.app_configured() {
        return (
            StatusCode::BAD_REQUEST,
            "connect a GitHub App before adding a private repository",
        )
            .into_response();
    }

    let port: u16 = match form.port.trim() {
        "" => 0,
        raw => match raw.parse() {
            Ok(p) => p,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "port must be a number between 0 and 65535")
                    .into_response()
            }
        },
    };

    let branch = match form.branch.trim() {
        "" => "main".to_string(),
        b => b.to_string(),
    };
    let strategy = match form.strategy.trim() {
        "" => "Static files".to_string(),
        s => s.to_string(),
    };

    let (tx, rx) = verdict_channel();
    let sent = state.conn.reducers.register_application_then(
        form.name.trim().to_string(),
        kind,
        form.repo.trim().to_string(),
        branch,
        form.entrypoint.trim().to_string(),
        port,
        form.deploy_path.trim().to_string(),
        strategy,
        move |_ctx, outcome| {
            let _ = tx.send(outcome);
        },
    );
    match verdict(sent, rx).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct DeployForm {
    pub app_id: u64,
    pub host_id: u64,
    /// Branch, tag or commit. Resolved to a concrete sha before queueing, so
    /// the record says what was actually deployed.
    pub git_ref: Option<String>,
    pub strategy: String,
}

pub async fn deploy(State(state): State<AppState>, Form(form): Form<DeployForm>) -> Response {
    let app = state
        .conn
        .db
        .application()
        .iter()
        .find(|a| a.id == form.app_id);
    let Some(app) = app else {
        return (StatusCode::BAD_REQUEST, "no such application").into_response();
    };

    let reference = form
        .git_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or(&app.default_branch)
        .to_string();

    let private = app.source_kind == SourceKind::GitHubApp;
    let commit = match state.github.resolve_ref(&app.repo, &reference, private).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };

    queue(&state.conn, form.app_id, form.host_id, commit.sha, commit.message, form.strategy).await
}

async fn queue(
    conn: &DbConnection,
    app_id: u64,
    host_id: u64,
    sha: String,
    message: String,
    strategy: String,
) -> Response {
    let (tx, rx) = verdict_channel();
    let sent = conn.reducers.queue_deployment_then(
        app_id,
        host_id,
        sha,
        message,
        strategy,
        move |_ctx, outcome| {
            let _ = tx.send(outcome);
        },
    );
    match verdict(sent, rx).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct DrainForm {
    pub host_id: u64,
    pub draining: bool,
}

pub async fn drain(State(state): State<AppState>, Form(form): Form<DrainForm>) -> Response {
    let (tx, rx) = verdict_channel();
    let sent = state.conn.reducers.set_host_draining_then(
        form.host_id,
        form.draining,
        move |_ctx, outcome| {
            let _ = tx.send(outcome);
        },
    );
    match verdict(sent, rx).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct RollbackForm {
    pub deployment_id: u64,
}

/// Roll back: mark the deployment, then put the previous release back.
///
/// A rollback that only marks the record is a lie — the operator pressed the
/// button because they want the old code running.
pub async fn rollback(State(state): State<AppState>, Form(form): Form<RollbackForm>) -> Response {
    let (dep, prior) = {
        let db = &state.conn.db;
        let Some(dep) = db.deployment().iter().find(|d| d.id == form.deployment_id) else {
            return (StatusCode::NOT_FOUND, "no such deployment").into_response();
        };
        // The most recent earlier success of the same app on the same host
        // with a *different* commit — rolling back to the identical release
        // would change nothing.
        let prior = db
            .deployment()
            .iter()
            .filter(|p| {
                p.app_id == dep.app_id
                    && p.host_id == dep.host_id
                    && p.id < dep.id
                    && p.status == DeployStatus::Succeeded
                    && p.commit_sha != dep.commit_sha
            })
            .max_by_key(|p| p.id);
        (dep, prior)
    };

    let Some(prior) = prior else {
        return (
            StatusCode::BAD_REQUEST,
            "no earlier successful release of this application on this host to roll back to",
        )
            .into_response();
    };

    let (tx, rx) = verdict_channel();
    let sent = state
        .conn
        .reducers
        .rollback_deployment_then(dep.id, move |_ctx, outcome| {
            let _ = tx.send(outcome);
        });
    if let Err(response) = verdict(sent, rx).await {
        return response;
    }

    let short = crate::fleet::short_sha(&prior.commit_sha);
    queue(
        &state.conn,
        dep.app_id,
        dep.host_id,
        prior.commit_sha.clone(),
        format!("Rollback to {short}"),
        prior.strategy.clone(),
    )
    .await
}

#[derive(Deserialize)]
pub struct DeleteAppForm {
    pub app_id: u64,
}

/// Remove an application: the control plane record first, then its stored
/// environment. Deployment history stays, and nothing running is touched.
pub async fn delete_app(State(state): State<AppState>, Form(form): Form<DeleteAppForm>) -> Response {
    let name = state
        .conn
        .db
        .application()
        .iter()
        .find(|a| a.id == form.app_id)
        .map(|a| a.name.clone());

    let (tx, rx) = verdict_channel();
    let sent = state
        .conn
        .reducers
        .delete_application_then(form.app_id, move |_ctx, outcome| {
            let _ = tx.send(outcome);
        });
    if let Err(response) = verdict(sent, rx).await {
        return response;
    }

    if let Some(name) = name {
        if let Err(e) = state.store.clear_env(&name).await {
            return internal(e);
        }
    }
    Redirect::to("/applications").into_response()
}

#[derive(Deserialize)]
pub struct DeleteHostForm {
    pub host_id: u64,
}

pub async fn remove_host(State(state): State<AppState>, Form(form): Form<DeleteHostForm>) -> Response {
    let (tx, rx) = verdict_channel();
    let sent = state
        .conn
        .reducers
        .delete_host_then(form.host_id, move |_ctx, outcome| {
            let _ = tx.send(outcome);
        });
    match verdict(sent, rx).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(response) => response,
    }
}

// --------------------------------------------------------- users and tokens

fn require_admin(user: &CurrentUser) -> Result<(), Response> {
    if user.admin {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "administrators only").into_response())
    }
}

#[derive(Deserialize)]
pub struct AddUserForm {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub admin: Option<String>,
}

pub async fn team_add(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(form): Form<AddUserForm>,
) -> Response {
    if let Err(resp) = require_admin(&user) {
        return resp;
    }
    if let Err(resp) = password_acceptable(&form.password) {
        return resp;
    }
    match state
        .store
        .add_user(&form.username, &form.password, form.admin.is_some())
        .await
    {
        Ok(_) => Redirect::to("/settings/team").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ResetPasswordForm {
    pub user: String,
    pub password: String,
}

pub async fn team_reset(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(form): Form<ResetPasswordForm>,
) -> Response {
    if let Err(resp) = require_admin(&user) {
        return resp;
    }
    if let Err(resp) = password_acceptable(&form.password) {
        return resp;
    }
    match state.store.set_user_password(&form.user, &form.password).await {
        Ok(()) => Redirect::to("/settings/team").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct RemoveUserForm {
    pub user: String,
}

pub async fn team_remove(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(form): Form<RemoveUserForm>,
) -> Response {
    if let Err(resp) = require_admin(&user) {
        return resp;
    }
    if form.user.trim().to_lowercase() == user.name {
        return (StatusCode::BAD_REQUEST, "you cannot remove your own account").into_response();
    }
    match state.store.remove_user(&form.user).await {
        Ok(()) => Redirect::to("/settings/team").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    pub current: String,
    pub password: String,
}

/// Any signed-in person may rotate their own password — proving the current
/// one first, so a walked-away-from browser cannot silently take the account.
pub async fn change_password(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(form): Form<ChangePasswordForm>,
) -> Response {
    if user.via_token {
        return (StatusCode::FORBIDDEN, "API tokens cannot change passwords").into_response();
    }
    if state.store.verify_login(&user.name, &form.current).await.is_none() {
        return (StatusCode::BAD_REQUEST, "the current password is wrong").into_response();
    }
    if let Err(resp) = password_acceptable(&form.password) {
        return resp;
    }
    match state.store.set_user_password(&user.name, &form.password).await {
        Ok(()) => (StatusCode::OK, "password changed").into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MintTokenForm {
    pub name: String,
}

/// Mint an API token. Shown exactly once; only the hash is kept.
pub async fn token_mint(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(form): Form<MintTokenForm>,
) -> Response {
    if let Err(resp) = require_admin(&user) {
        return resp;
    }
    match state.store.mint_api_token(&form.name).await {
        Ok(token) => (
            StatusCode::OK,
            format!("{token}\n\nThis token is shown once. Use it as:  Authorization: Bearer <token>"),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct RevokeTokenForm {
    pub name: String,
}

pub async fn token_revoke(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(form): Form<RevokeTokenForm>,
) -> Response {
    if let Err(resp) = require_admin(&user) {
        return resp;
    }
    match state.store.revoke_api_token(&form.name).await {
        Ok(()) => Redirect::to("/settings/tokens").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

// ------------------------------------------------------------------- GitHub

#[derive(Deserialize)]
pub struct GitHubForm {
    pub app_id: String,
    pub private_key_pem: String,
    pub webhook_secret: String,
}

/// Store the instance's GitHub App credentials — on the server's disk, never
/// in SpacetimeDB.
pub async fn connect_github(State(state): State<AppState>, Form(form): Form<GitHubForm>) -> Response {
    let config = AppConfig {
        app_id: form.app_id.trim().to_string(),
        private_key_pem: form.private_key_pem.trim().to_string(),
        webhook_secret: Some(form.webhook_secret.trim().to_string()).filter(|s| !s.is_empty()),
    };

    if config.app_id.is_empty() || config.private_key_pem.is_empty() {
        return (StatusCode::BAD_REQUEST, "App ID and private key are both required")
            .into_response();
    }
    if config.app_id.chars().any(|c| !c.is_ascii_digit()) {
        return (
            StatusCode::BAD_REQUEST,
            "the App ID is the numeric id from the App's settings page",
        )
            .into_response();
    }
    if let Err(e) = crate::github::GitHub::validate_key(&config.private_key_pem) {
        return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response();
    }

    if let Err(e) = state.store.set_github(Some(config)).await {
        return internal(e);
    }
    // The live GitHub client caches tokens keyed to the old credentials; a
    // restart is the honest way to pick new ones up.
    Redirect::to("/?github=saved").into_response()
}

/// Forget the GitHub App credentials.
pub async fn disconnect_github(State(state): State<AppState>) -> Response {
    if let Err(e) = state.store.set_github(None).await {
        return internal(e);
    }
    Redirect::to("/").into_response()
}

/// Repositories the connected GitHub App can see.
pub async fn list_repos(State(state): State<AppState>) -> Response {
    match state.github.repositories().await {
        Ok(repos) => Json(repos).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct DetectForm {
    pub repo: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub visibility: String,
}

/// Look at a repository and propose how to deploy it.
pub async fn detect(State(state): State<AppState>, Form(form): Form<DetectForm>) -> Response {
    let repo = form.repo.trim();
    if repo.is_empty() {
        return (StatusCode::BAD_REQUEST, "give a repository as owner/name").into_response();
    }

    let private = form.visibility == "private";
    if private && !state.github.app_configured() {
        return (
            StatusCode::BAD_REQUEST,
            "connect a GitHub App before inspecting a private repository",
        )
            .into_response();
    }

    let branch = match form.branch.trim() {
        "" => "HEAD".to_string(),
        b => b.to_string(),
    };

    match state.github.root_files(repo, &branch, private).await {
        Ok(files) => Json(crate::detect::from_root_files(&files)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

// -------------------------------------------------------------- environment

#[derive(Deserialize)]
pub struct EnvForm {
    pub app: String,
    /// `KEY=value` lines, `.env` style.
    pub vars: String,
    /// Present when the "store as secrets" box is ticked.
    #[serde(default)]
    pub secret: Option<String>,
}

pub async fn set_env(State(state): State<AppState>, Form(form): Form<EnvForm>) -> Response {
    let app = form.app.trim();
    if app.is_empty() {
        return (StatusCode::BAD_REQUEST, "choose an application").into_response();
    }

    let secret = form.secret.is_some();
    let vars = match parse_env(&form.vars, secret) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    if vars.is_empty() {
        return (StatusCode::BAD_REQUEST, "nothing to set").into_response();
    }

    let count = vars.len();
    if let Err(e) = state.store.set_env(app, vars).await {
        return internal(e);
    }
    // Names only, never values: a secret must not end up in a browser history
    // entry or a screenshot.
    (StatusCode::OK, format!("set {count} variable(s) on {app}")).into_response()
}

#[derive(Deserialize)]
pub struct UnsetEnvForm {
    pub app: String,
    pub key: String,
}

pub async fn unset_env(State(state): State<AppState>, Form(form): Form<UnsetEnvForm>) -> Response {
    if let Err(e) = state.store.remove_env(form.app.trim(), form.key.trim()).await {
        return internal(e);
    }
    Redirect::to("/").into_response()
}

// -------------------------------------------------------------------- agents

#[derive(Deserialize)]
pub struct EnrollRequest {
    pub code: String,
    pub host_name: String,
}

#[derive(Serialize)]
pub struct EnrollResponse {
    pub agent_token: String,
    pub spacetime_uri: String,
    pub database: String,
}

/// Exchange an enrolment code for a long-lived agent token plus the control
/// plane's address. Single-use here as well as in the control plane.
pub async fn enroll(State(state): State<AppState>, Json(req): Json<EnrollRequest>) -> Response {
    match state
        .store
        .redeem_enrollment(req.code.trim(), req.host_name.trim())
        .await
    {
        Ok(Some(agent_token)) => Json(EnrollResponse {
            agent_token,
            spacetime_uri: state.spacetime_uri.clone(),
            database: state.database.clone(),
        })
        .into_response(),
        Ok(None) => {
            (StatusCode::FORBIDDEN, "unknown or already-used enrolment code").into_response()
        }
        Err(e) => internal(e),
    }
}

fn bearer(req: &Request) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// Resolve which deployment an agent may touch: the token must be valid and
/// the deployment must target the host it was issued for. Without this, any
/// agent token would open every private repository and every application's
/// secrets this instance can reach.
async fn agent_owned_deployment(
    state: &AppState,
    deployment_id: u64,
    token: Option<String>,
) -> Result<crate::module_bindings::Deployment, Response> {
    // The token comes in pre-extracted: holding &Request across an await would
    // make the handler future non-Send, because the request body is not Sync.
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "agent token required").into_response());
    };
    let Some(host_name) = state.store.agent_for(&token).await else {
        return Err((StatusCode::FORBIDDEN, "unrecognised agent token").into_response());
    };

    let db = &state.conn.db;
    let Some(deployment) = db.deployment().iter().find(|d| d.id == deployment_id) else {
        return Err((StatusCode::NOT_FOUND, "no such deployment").into_response());
    };
    let owns_it = db
        .host()
        .iter()
        .any(|h| h.id == deployment.host_id && h.name == host_name);
    if !owns_it {
        return Err(
            (StatusCode::FORBIDDEN, "this deployment belongs to a different host").into_response(),
        );
    }
    Ok(deployment)
}

/// Stream a deployment's source to the agent that owns it. This is why agents
/// never hold GitHub credentials: the token is minted here, used here, and
/// never leaves.
pub async fn deployment_source(
    State(state): State<AppState>,
    Path(deployment_id): Path<u64>,
    req: Request,
) -> Response {
    let token = bearer(&req);
    drop(req);
    let deployment = match agent_owned_deployment(&state, deployment_id, token).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    let app = state
        .conn
        .db
        .application()
        .iter()
        .find(|a| a.id == deployment.app_id);
    let Some(app) = app else {
        return (StatusCode::NOT_FOUND, "deployment references a missing application")
            .into_response();
    };

    let private = app.source_kind == SourceKind::GitHubApp;
    let upstream = match state
        .github
        .tarball(&app.repo, &deployment.commit_sha, private)
        .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };

    // Stream rather than buffer: a large repository should never have to fit
    // in the server's memory on its way to an agent.
    (
        [
            (header::CONTENT_TYPE, "application/gzip"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(upstream.bytes_stream()),
    )
        .into_response()
}

/// The environment for a deployment, for the agent that owns it.
pub async fn deployment_env(
    State(state): State<AppState>,
    Path(deployment_id): Path<u64>,
    req: Request,
) -> Response {
    let token = bearer(&req);
    drop(req);
    let deployment = match agent_owned_deployment(&state, deployment_id, token).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    let app_name = state
        .conn
        .db
        .application()
        .iter()
        .find(|a| a.id == deployment.app_id)
        .map(|a| a.name.clone());
    let Some(app_name) = app_name else {
        return (StatusCode::NOT_FOUND, "deployment references a missing application")
            .into_response();
    };

    let env: std::collections::BTreeMap<String, String> = state
        .store
        .env_for(&app_name)
        .await
        .into_iter()
        .map(|v| (v.key, v.value))
        .collect();

    ([(header::CACHE_CONTROL, "no-store")], Json(env)).into_response()
}

// -------------------------------------------------------------------- shared

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
}

/// Human-typeable: an operator often reads this off a screen into a terminal,
/// so no glyphs that get misread (o/0, i/l/1).
fn generate_enrollment_code() -> anyhow::Result<String> {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("reading randomness: {e}"))?;
    let body: String = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    Ok(format!("{}-{}-{}", &body[0..4], &body[4..8], &body[8..12]))
}

/// Called once on startup so the server, not an agent, owns operator rights.
pub fn claim(conn: &DbConnection) {
    if let Err(e) = conn.reducers.claim_instance() {
        tracing::warn!("could not claim instance: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_codes_are_readable_and_unique() {
        let a = generate_enrollment_code().unwrap();
        let b = generate_enrollment_code().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 14, "expected xxxx-xxxx-xxxx, got {a}");
        assert!(!a.contains(['o', 'i', 'l', '0', '1']), "ambiguous glyph in {a}");
    }

    #[tokio::test]
    async fn sessions_belong_to_a_user_and_revoke() {
        let sessions = Sessions::default();
        let token = sessions.create("morgan").await.unwrap();
        assert_eq!(sessions.user_for(&token).await.as_deref(), Some("morgan"));
        assert_eq!(sessions.user_for("forged").await, None);
        sessions.revoke(&token).await;
        assert_eq!(sessions.user_for(&token).await, None);
    }
}
