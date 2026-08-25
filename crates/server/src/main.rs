//! DigiHost server.
//!
//! This is the thing a customer installs. It holds the connection to its own
//! SpacetimeDB instance (optionally supervising the process itself), renders
//! the web interface, brokers source and configuration to agents, and pushes
//! fleet changes to open browsers over SSE. Browsers never talk to
//! SpacetimeDB, and agents never hold a source credential.

mod api;
mod detect;
mod fleet;
mod github;
mod module_bindings;
mod store;
mod supervise;
mod view;
mod webhook;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Extension, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Router};
use clap::Parser;
use futures::stream::Stream;
use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};
use tokio::sync::broadcast;

use api::{CurrentUser, Sessions};
use github::GitHub;
use module_bindings::{
    application_table::ApplicationTableAccess, deploy_log_line_table::DeployLogLineTableAccess,
    deployment_table::DeploymentTableAccess, host_table::HostTableAccess,
    workload_table::WorkloadTableAccess, DbConnection, SourceKind,
};
use store::Store;

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where update checks look. Empty disables the check entirely.
const DEFAULT_UPDATE_REPO: &str = "monkzoren/digihost";

#[derive(Parser, Debug)]
#[command(name = "digihost-server", version, about = "DigiHost: the web interface and control plane host")]
struct Args {
    /// Address the web interface listens on.
    #[arg(long, env = "DIGIHOST_BIND", default_value = "127.0.0.1:8420")]
    bind: SocketAddr,

    /// URL agents should use to reach this server. Shown in install commands.
    #[arg(long, env = "DIGIHOST_PUBLIC_URL")]
    public_url: Option<String>,

    /// SpacetimeDB instance backing this DigiHost.
    #[arg(long, env = "DIGIHOST_SPACETIME_URI", default_value = "http://127.0.0.1:3000")]
    spacetime_uri: String,

    /// Database name published on that instance.
    #[arg(long, env = "DIGIHOST_DATABASE", default_value = "digihost")]
    database: String,

    /// Where the server keeps its own state: operator credential, agent
    /// tokens, GitHub App key, application environment. Never in SpacetimeDB.
    #[arg(long, env = "DIGIHOST_DATA_DIR", default_value = ".digihost")]
    data_dir: PathBuf,

    /// Name shown in the sidebar — this instance's identity to its operators.
    #[arg(long, env = "DIGIHOST_INSTANCE", default_value = "DigiHost instance")]
    instance: String,

    /// Start and supervise SpacetimeDB rather than expecting one running.
    #[arg(long, env = "DIGIHOST_MANAGE_SPACETIME")]
    manage_spacetime: bool,

    /// Compiled control-plane module. When given, DigiHost publishes it on
    /// startup — on first install, and again whenever the file changes, which
    /// is how updates carry schema changes.
    #[arg(long, env = "DIGIHOST_MODULE_WASM")]
    module_wasm: Option<PathBuf>,

    /// GitHub repository checked for newer releases. Empty disables the check.
    #[arg(long, env = "DIGIHOST_UPDATE_REPO", default_value = DEFAULT_UPDATE_REPO)]
    update_repo: String,
}

/// "0.2.0" is newer than "0.1.9"; anything unparseable is not newer.
fn release_is_newer(latest: &str, current: &str) -> bool {
    fn triple(v: &str) -> Option<(u64, u64, u64)> {
        let mut it = v.trim().trim_start_matches('v').splitn(3, '.');
        Some((
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next().unwrap_or("0").parse().ok()?,
        ))
    }
    match (triple(latest), triple(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Ask GitHub for the newest release tag every few hours. Best effort: any
/// failure just means no banner until the next try.
fn spawn_update_check(repo: String, slot: Arc<tokio::sync::RwLock<Option<String>>>) {
    if repo.trim().is_empty() {
        return;
    }
    tokio::spawn(async move {
        let http = match reqwest::Client::builder()
            .user_agent(concat!("DigiHost/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            let url = format!("https://api.github.com/repos/{repo}/releases/latest");
            if let Ok(resp) = http.get(&url).send().await {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(tag) = body.get("tag_name").and_then(|t| t.as_str()) {
                            let version = tag.trim_start_matches('v').to_string();
                            *slot.write().await = Some(version);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });
}

/// Publish the control plane when it is missing, and republish it when the
/// shipped wasm changes — which is how a binary update carries its schema.
///
/// The wasm's hash is recorded in the data directory after every successful
/// publish; matching hash means nothing to do. A failed upgrade publish stops
/// startup deliberately: new binaries against an old schema would fail in
/// stranger ways later.
async fn ensure_module_published(
    uri: &str,
    database: &str,
    wasm: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    let bytes = tokio::fs::read(wasm)
        .await
        .with_context(|| format!("reading {}", wasm.display()))?;
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let marker = data_dir.join("module.sha256");
    let recorded = tokio::fs::read_to_string(&marker)
        .await
        .ok()
        .map(|s| s.trim().to_string());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let exists = match http
        .get(format!("{}/v1/database/{database}", uri.trim_end_matches('/')))
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    };

    if exists && recorded.as_deref() == Some(hash.as_str()) {
        return Ok(());
    }
    if exists {
        tracing::info!("control plane module changed; upgrading the schema");
    } else {
        tracing::info!("control plane not published yet; publishing {}", wasm.display());
    }

    let status = tokio::process::Command::new("spacetime")
        .args(["publish", "--server", uri, "-y", "--bin-path"])
        .arg(wasm)
        .arg(database)
        .status()
        .await
        .context("running `spacetime publish` — is the spacetime CLI on PATH?")?;
    if !status.success() {
        if exists {
            anyhow::bail!(
                "upgrading the control plane failed (exit {:?}). The new module likely \
                 contains breaking schema changes; resolve manually with `spacetime publish` \
                 before restarting DigiHost.",
                status.code()
            );
        }
        anyhow::bail!("publishing the control plane failed (exit {:?})", status.code());
    }

    tokio::fs::write(&marker, &hash)
        .await
        .context("recording the published module hash")?;
    Ok(())
}

#[derive(Clone)]
pub struct AppState {
    pub conn: Arc<DbConnection>,
    pub latest_release: Arc<tokio::sync::RwLock<Option<String>>>,
    pub instance: String,
    pub changes: broadcast::Sender<()>,
    pub store: Store,
    pub github: GitHub,
    pub sessions: Sessions,
    pub public_url: String,
    pub spacetime_uri: String,
    pub database: String,
}

impl AppState {
    /// The newest published release, when it is actually newer than this build.
    async fn update_available(&self) -> Option<String> {
        let latest = self.latest_release.read().await.clone()?;
        release_is_newer(&latest, SERVER_VERSION).then_some(latest)
    }

    /// Read the client cache and derive everything the fleet page needs.
    fn snapshot(&self) -> fleet::FleetSnapshot {
        let db = &self.conn.db;
        let apps: HashMap<u64, String> = db
            .application()
            .iter()
            .map(|a| (a.id, a.name.clone()))
            .collect();
        fleet::build(
            db.host().iter().collect(),
            db.workload().iter().collect(),
            db.deployment().iter().collect(),
            apps,
        )
    }

    /// Applications with the strategy each normally uses, so the deploy dialog
    /// offers the right one instead of a fixed default.
    fn app_strategies(&self) -> Vec<(u64, String, String)> {
        let mut apps: Vec<(u64, String, String)> = self
            .conn
            .db
            .application()
            .iter()
            .map(|a| (a.id, a.name.clone(), a.default_strategy.clone()))
            .collect();
        apps.sort_by(|a, b| a.1.cmp(&b.1));
        apps
    }

    fn host_choices(&self) -> Vec<(u64, String)> {
        let mut hosts: Vec<(u64, String)> = self
            .conn
            .db
            .host()
            .iter()
            .map(|h| (h.id, h.name.clone()))
            .collect();
        hosts.sort_by(|a, b| a.1.cmp(&b.1));
        hosts
    }

    /// Applications with their currently-set environment, for the env editor.
    async fn app_env(&self) -> Vec<(String, Vec<store::EnvVar>)> {
        let names: Vec<String> = self.app_strategies().into_iter().map(|(_, n, _)| n).collect();
        let mut out = Vec::new();
        for name in names {
            let vars = self.store.env_for(&name).await;
            out.push((name, vars));
        }
        out
    }

    async fn application_overviews(&self) -> Vec<view::AppOverview> {
        let apps: Vec<_> = self.conn.db.application().iter().collect();
        let mut out = Vec::new();
        for a in apps {
            let last = self
                .conn
                .db
                .deployment()
                .iter()
                .filter(|d| d.app_id == a.id)
                .max_by_key(|d| d.id);
            let (last_status, last_tone) = match &last {
                Some(d) => fleet::deploy_state(d.status),
                None => ("Never deployed", fleet::Tone::Idle),
            };
            out.push(view::AppOverview {
                id: a.id,
                name: a.name.clone(),
                repo: a.repo.clone(),
                branch: a.default_branch.clone(),
                private: a.source_kind == SourceKind::GitHubApp,
                strategy: a.default_strategy.clone(),
                entrypoint: a.entrypoint.clone(),
                port: a.port,
                deploy_path: a.deploy_path.clone(),
                env_count: self.store.env_for(&a.name).await.len(),
                last_status,
                last_tone,
            });
        }
        out.sort_by(|x, y| x.name.cmp(&y.name));
        out
    }
}

async fn index(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Html<String> {
    let snap = state.snapshot();
    Html(
        view::page(
            &snap,
            &state.instance,
            &state.app_strategies(),
            &state.host_choices(),
            &state.app_env().await,
            state.github.app_configured(),
            SERVER_VERSION,
            state.update_available().await.as_deref(),
            &user.name,
        )
        .into_string(),
    )
}

async fn applications(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Html<String> {
    let snap = state.snapshot();
    Html(
        view::applications_page(
            &snap,
            &state.instance,
            &state.application_overviews().await,
            &state.app_strategies(),
            &state.host_choices(),
            &state.app_env().await,
            state.github.app_configured(),
            SERVER_VERSION,
            state.update_available().await.as_deref(),
            &user.name,
        )
        .into_string(),
    )
}

async fn team(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Html<String> {
    let snap = state.snapshot();
    Html(
        view::team_page(
            &snap,
            &state.instance,
            state.github.app_configured(),
            SERVER_VERSION,
            state.update_available().await.as_deref(),
            &user.name,
            user.admin,
            &state.store.users().await,
        )
        .into_string(),
    )
}

async fn tokens(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Html<String> {
    let snap = state.snapshot();
    Html(
        view::tokens_page(
            &snap,
            &state.instance,
            state.github.app_configured(),
            SERVER_VERSION,
            state.update_available().await.as_deref(),
            &user.name,
            user.admin,
            &state.store.api_tokens().await,
        )
        .into_string(),
    )
}

async fn login_page(State(state): State<AppState>) -> Response {
    if !state.store.is_claimed().await {
        return axum::response::Redirect::to("/setup").into_response();
    }
    Html(view::login(&state.instance, false).into_string()).into_response()
}

async fn setup_page(State(state): State<AppState>) -> Response {
    if state.store.is_claimed().await {
        return axum::response::Redirect::to("/login").into_response();
    }
    Html(view::login(&state.instance, true).into_string()).into_response()
}

/// One deployment's detail and streamed output, as a markup fragment for the
/// drawer. Rendered server-side like everything else.
async fn deployment_log(
    State(state): State<AppState>,
    axum::extract::Path(deployment_id): axum::extract::Path<u64>,
) -> Response {
    let db = &state.conn.db;
    let Some(deployment) = db.deployment().iter().find(|d| d.id == deployment_id) else {
        return (axum::http::StatusCode::NOT_FOUND, "no such deployment").into_response();
    };

    let app = db.application().iter().find(|a| a.id == deployment.app_id);
    let host = db.host().iter().find(|h| h.id == deployment.host_id);
    let lines = db
        .deploy_log_line()
        .iter()
        .filter(|l| l.deployment_id == deployment_id)
        .collect();

    let detail = fleet::build_detail(&deployment, app.as_ref(), host.as_ref(), lines);
    Html(view::deploy_log(&detail).into_string()).into_response()
}

/// One SSE stream per open browser. Each fleet change re-renders the body
/// server-side and ships the markup — the client never rebuilds a row itself.
async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.changes.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(()) => {}
                // Lagging only means intermediate states were skipped; the
                // next render is a full snapshot anyway.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }

            let snap = state.snapshot();
            let payload = serde_json::json!({
                "html": view::fleet_body(&snap).into_string(),
                "summary": snap.summary(),
            });
            yield Ok(Event::default().event("fleet").data(payload.to_string()));
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn load_server_token(dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(dir.join("server-token"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "digihost_server=info".into()),
        )
        .init();

    let args = Args::parse();
    let store = Store::open(&args.data_dir).await?;
    let github = GitHub::new(store.github().await)?;

    // Optionally own the database process, so the installable unit is one
    // thing rather than two.
    let _spacetime = if args.manage_spacetime {
        Some(supervise::Spacetime::start(&args.spacetime_uri).await?)
    } else {
        None
    };

    if let Some(wasm) = &args.module_wasm {
        ensure_module_published(&args.spacetime_uri, &args.database, wasm, &args.data_dir).await?;
    }

    let (changes, _) = broadcast::channel(64);
    let connected = Arc::new(AtomicBool::new(false));

    let conn = {
        let flag = connected.clone();
        let dir = args.data_dir.clone();
        DbConnection::builder()
            .with_uri(&args.spacetime_uri)
            .with_database_name(&args.database)
            .with_token(load_server_token(&args.data_dir))
            .on_connect(move |_ctx, _identity, token| {
                // Persisted so the server keeps the same identity across
                // restarts — the control plane's operator claim is bound to
                // it, and a new identity would be refused.
                if let Err(e) = std::fs::write(dir.join("server-token"), token) {
                    tracing::warn!("could not persist server token: {e}");
                }
                flag.store(true, Ordering::SeqCst);
                tracing::info!("connected to SpacetimeDB");
            })
            .on_connect_error(|_ctx, err| tracing::error!("SpacetimeDB connection failed: {err}"))
            .on_disconnect(|_ctx, err| match err {
                Some(e) => tracing::warn!("SpacetimeDB disconnected: {e}"),
                None => tracing::info!("SpacetimeDB disconnected"),
            })
            .build()
            .context("building SpacetimeDB connection")?
    };

    // Any row change in any subscribed table is a fleet change. Re-rendering
    // the whole body is cheap at this scale and keeps diffing logic at zero.
    let notify = changes.clone();
    let bump = move || {
        let _ = notify.send(());
    };
    {
        let db = &conn.db;
        let (a, b, c) = (bump.clone(), bump.clone(), bump.clone());
        db.host().on_insert(move |_, _| a());
        db.host().on_update(move |_, _, _| b());
        db.host().on_delete(move |_, _| c());

        let (a, b, c) = (bump.clone(), bump.clone(), bump.clone());
        db.workload().on_insert(move |_, _| a());
        db.workload().on_delete(move |_, _| b());
        db.deployment().on_insert(move |_, _| c());

        let (a, b, c) = (bump.clone(), bump.clone(), bump.clone());
        db.deployment().on_update(move |_, _, _| a());
        db.application().on_insert(move |_, _| b());
        // Log lines drive the open deployment drawer; without this the log
        // sits still while the agent is mid-deploy.
        db.deploy_log_line().on_insert(move |_, _| c());
    }

    conn.subscription_builder()
        .on_applied(|_ctx| tracing::info!("fleet state subscribed"))
        .on_error(|_ctx, err| tracing::error!("subscription failed: {err}"))
        .subscribe_to_all_tables();

    conn.run_threaded();

    for _ in 0..150 {
        if connected.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !connected.load(Ordering::SeqCst) {
        anyhow::bail!(
            "could not reach SpacetimeDB at {} — is the instance running? \
             (pass --manage-spacetime to have DigiHost start one)",
            args.spacetime_uri
        );
    }
    // Subscription rows land just after connect; without this the first page
    // load can render an empty fleet that only fills in on the next change.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Take operator rights before serving anything, so an agent cannot.
    api::claim(&conn);

    let public_url = args
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", args.bind));

    let latest_release = Arc::new(tokio::sync::RwLock::new(None));
    spawn_update_check(args.update_repo.clone(), Arc::clone(&latest_release));

    let state = AppState {
        conn: Arc::new(conn),
        latest_release,
        instance: args.instance.clone(),
        changes,
        store,
        github,
        sessions: Sessions::default(),
        public_url,
        spacetime_uri: args.spacetime_uri.clone(),
        database: args.database.clone(),
    };

    // Everything an operator sees or does sits behind the session gate. The
    // agent API authenticates per request with its own bearer token instead.
    let protected = Router::new()
        .route("/", get(index))
        .route("/applications", get(applications))
        .route("/settings/team", get(team))
        .route("/settings/tokens", get(tokens))
        .route("/events", get(events))
        .route("/deployments/{deployment_id}/log", get(deployment_log))
        .route("/logout", post(api::logout))
        .route("/actions/add-server", post(api::add_server))
        .route("/actions/register-app", post(api::register_app))
        .route("/actions/deploy", post(api::deploy))
        .route("/actions/drain", post(api::drain))
        .route("/actions/rollback", post(api::rollback))
        .route("/actions/delete-app", post(api::delete_app))
        .route("/actions/delete-host", post(api::remove_host))
        .route("/actions/github", post(api::connect_github))
        .route("/actions/github/disconnect", post(api::disconnect_github))
        .route("/actions/env", post(api::set_env))
        .route("/actions/env/unset", post(api::unset_env))
        .route("/actions/team/add", post(api::team_add))
        .route("/actions/team/reset", post(api::team_reset))
        .route("/actions/team/remove", post(api::team_remove))
        .route("/actions/password", post(api::change_password))
        .route("/actions/tokens/mint", post(api::token_mint))
        .route("/actions/tokens/revoke", post(api::token_revoke))
        .route("/actions/repos", get(api::list_repos))
        .route("/actions/detect", post(api::detect))
        .layer(middleware::from_fn_with_state(state.clone(), api::require_operator));

    let public = Router::new()
        .route("/login", get(login_page).post(api::login))
        .route("/setup", get(setup_page).post(api::setup))
        .route("/api/enroll", post(api::enroll))
        .route("/api/github/webhook", post(webhook::receive))
        .route("/api/deployments/{deployment_id}/source", get(api::deployment_source))
        .route("/api/deployments/{deployment_id}/env", get(api::deployment_env));

    let app = protected.merge(public).with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    tracing::info!("DigiHost interface on http://{}", args.bind);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await
        .context("serving")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::release_is_newer;

    #[test]
    fn version_comparison_is_numeric_not_lexical() {
        assert!(release_is_newer("0.2.0", "0.1.0"));
        assert!(release_is_newer("0.10.0", "0.9.9"), "10 > 9 numerically");
        assert!(release_is_newer("1.0.0", "0.99.99"));
        assert!(!release_is_newer("0.1.0", "0.1.0"));
        assert!(!release_is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn tags_and_garbage_are_handled() {
        assert!(release_is_newer("v0.2.0", "0.1.0"), "a leading v is fine");
        assert!(!release_is_newer("nightly", "0.1.0"), "unparseable is never newer");
        assert!(!release_is_newer("", "0.1.0"));
    }
}
