//! DigiHost agent.
//!
//! Runs on a managed Linux or Windows host. Bootstraps from a single
//! enrolment code: it trades the code with DigiHost Server for its own bearer
//! token and the control plane's address, enrols itself, then reports load and
//! runs deployments for as long as it lives.
//!
//! Identity follows the state directory — the SpacetimeDB token is stored
//! there, so two agents with different state dirs are two identities, and a
//! second agent pointed at the same state dir is the same host on purpose.

mod deploy;
mod module_bindings;
mod probe;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use spacetimedb_sdk::{DbContext, Table};
use sysinfo::System;

use module_bindings::{
    application_table::ApplicationTableAccess, deployment_table::DeploymentTableAccess,
    enroll_host, heartbeat, host_table::HostTableAccess, DbConnection, DeployStatus,
};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "digihost-agent", version, about = "Reports this host to a DigiHost instance and deploys to it")]
struct Args {
    /// URL of the DigiHost server. The agent asks it where the control plane
    /// lives and fetches deployment source and configuration through it.
    #[arg(long, env = "DIGIHOST_SERVER", default_value = "http://127.0.0.1:8420")]
    server: String,

    /// Single-use enrolment code, required on first run only.
    #[arg(long, env = "DIGIHOST_ENROLLMENT_CODE")]
    enrollment_code: Option<String>,

    /// Override the reported host name (defaults to the machine's hostname).
    #[arg(long)]
    host_name: Option<String>,

    /// Seconds between heartbeats.
    #[arg(long, default_value_t = 20)]
    interval: u64,

    /// Also report OS and vendor services, not only DigiHost-deployed
    /// workloads. Useful for discovery; noisy as a steady state.
    #[arg(long)]
    include_system_services: bool,

    /// Where the agent keeps its token, enrolment state and release
    /// directories.
    #[arg(long, env = "DIGIHOST_STATE_DIR")]
    state_dir: Option<PathBuf>,
}

/// Persisted so a restarted agent keeps its identity instead of trying to
/// enrol a second time with a spent code.
#[derive(Default, Serialize, Deserialize)]
struct AgentState {
    enrolled: bool,
    host_name: Option<String>,
    /// Bearer token for this agent's HTTP calls to the DigiHost server.
    #[serde(default)]
    agent_token: Option<String>,
    /// Where the control plane lives, as reported by the server at enrolment.
    #[serde(default)]
    spacetime_uri: Option<String>,
    #[serde(default)]
    database: Option<String>,
}

impl AgentState {
    fn path(dir: &PathBuf) -> PathBuf {
        dir.join("agent-state.json")
    }

    fn load(dir: &PathBuf) -> Self {
        std::fs::read_to_string(Self::path(dir))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self, dir: &PathBuf) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(Self::path(dir), serde_json::to_string_pretty(self)?)
            .context("writing agent state")
    }
}

#[derive(Serialize)]
struct EnrollRequest<'a> {
    code: &'a str,
    host_name: &'a str,
}

#[derive(Deserialize)]
struct EnrollResponse {
    agent_token: String,
    spacetime_uri: String,
    database: String,
}

/// Trade the enrolment code for an agent token and the control plane address.
/// Runs before any SpacetimeDB connection: the server is what tells us where
/// the control plane is.
fn exchange_code(server: &str, code: &str, host_name: &str) -> Result<EnrollResponse> {
    let url = format!("{}/api/enroll", server.trim_end_matches('/'));
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let resp = http
        .post(&url)
        .json(&EnrollRequest { code, host_name })
        .send()
        .with_context(|| format!("contacting the DigiHost server at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("enrolment refused ({status}): {body}");
    }
    resp.json().context("parsing the enrolment response")
}

struct Job {
    id: u64,
    app: String,
    sha: String,
    strategy: String,
    target: deploy::Target,
}

/// Queued deployments targeting this host, oldest first.
fn queued_for_me(conn: &DbConnection, host_name: &str) -> Vec<Job> {
    let db = &conn.db;
    let Some(host) = db.host().iter().find(|h| h.name == host_name) else {
        return Vec::new();
    };

    let mut jobs: Vec<Job> = db
        .deployment()
        .iter()
        .filter(|d| d.host_id == host.id && d.status == DeployStatus::Queued)
        .map(|d| {
            let app = db.application().iter().find(|a| a.id == d.app_id);
            Job {
                id: d.id,
                app: app
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| format!("app-{}", d.app_id)),
                sha: d.commit_sha.clone(),
                strategy: d.strategy.clone(),
                target: deploy::Target {
                    entrypoint: app.as_ref().map(|a| a.entrypoint.clone()).unwrap_or_default(),
                    port: app.as_ref().map(|a| a.port).unwrap_or(0),
                    deploy_path: app.as_ref().map(|a| a.deploy_path.clone()).unwrap_or_default(),
                },
            }
        })
        .collect();
    jobs.sort_by_key(|j| j.id);
    jobs
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "digihost_agent=info".into()),
        )
        .init();

    let args = Args::parse();
    let state_dir = args
        .state_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(".digihost-agent"));
    let mut state = AgentState::load(&state_dir);

    let facts = probe::facts(args.host_name.clone().or_else(|| state.host_name.clone()));
    tracing::info!(
        host = %facts.name,
        platform = ?facts.platform,
        os = %facts.os_name,
        runtime = %facts.runtime,
        address = %facts.address,
        "identified this machine"
    );

    if !state.enrolled && args.enrollment_code.is_none() {
        anyhow::bail!(
            "this host is not enrolled yet — pass --enrollment-code with a code minted by \
             the DigiHost instance (state dir: {})",
            state_dir.display()
        );
    }

    // First run: swap the code for an agent token before touching the control
    // plane. Later runs already know everything they need.
    if !state.enrolled {
        let code = args.enrollment_code.clone().expect("checked above");
        let issued = exchange_code(&args.server, &code, &facts.name)?;
        state.agent_token = Some(issued.agent_token);
        state.spacetime_uri = Some(issued.spacetime_uri);
        state.database = Some(issued.database);
        state.host_name = Some(facts.name.clone());
        state.save(&state_dir)?;
        tracing::info!("received agent credentials from the DigiHost server");
    }

    let spacetime_uri = state
        .spacetime_uri
        .clone()
        .context("no control plane address recorded; re-enrol this agent")?;
    let database = state
        .database
        .clone()
        .context("no database name recorded; re-enrol this agent")?;
    let agent_token = state
        .agent_token
        .clone()
        .context("no agent token recorded; re-enrol this agent")?;

    // Identity lives beside the rest of the agent's state, not in a global
    // per-user location: the state dir is the unit of identity.
    let token_path = state_dir.join("spacetime-token");
    let saved_identity = std::fs::read_to_string(&token_path)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    let connected = Arc::new(AtomicBool::new(false));
    let flag = connected.clone();

    let conn = DbConnection::builder()
        .with_uri(&spacetime_uri)
        .with_database_name(&database)
        .with_token(saved_identity)
        .on_connect({
            let token_path = token_path.clone();
            move |_ctx, _identity, token| {
                if let Some(parent) = token_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&token_path, token) {
                    tracing::warn!("could not persist the agent identity: {e}");
                }
                flag.store(true, Ordering::SeqCst);
                tracing::info!("connected to DigiHost");
            }
        })
        .on_connect_error(|_ctx, err| tracing::error!("connection failed: {err}"))
        .on_disconnect(|_ctx, err| match err {
            Some(e) => tracing::warn!("disconnected: {e}"),
            None => tracing::info!("disconnected"),
        })
        .build()
        .context("building SpacetimeDB connection")?;

    conn.run_threaded();

    // The builder returns before the socket is up; reducers sent early are lost.
    for _ in 0..100 {
        if connected.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !connected.load(Ordering::SeqCst) {
        anyhow::bail!("timed out connecting to {spacetime_uri}");
    }

    conn.subscription_builder()
        .on_applied(|_ctx| tracing::debug!("subscribed"))
        .on_error(|_ctx, err| tracing::error!("subscription failed: {err}"))
        .subscribe_to_all_tables();

    if !state.enrolled {
        let code = args.enrollment_code.clone().expect("checked above");
        conn.reducers.enroll_host(
            code,
            facts.name.clone(),
            facts.address.clone(),
            facts.platform,
            facts.os_name.clone(),
            facts.runtime.clone(),
            AGENT_VERSION.to_string(),
        )?;
        // Idempotent from our side: the control plane rejects a second attempt,
        // so recording it locally only prevents pointless retries.
        state.enrolled = true;
        state.save(&state_dir)?;
        tracing::info!(host = %facts.name, "enrolment requested");
    }

    let executor = deploy::Executor::new(
        args.server.clone(),
        agent_token,
        state_dir.join("work"),
    )?;

    // Deployments run on their own thread. A compose build takes minutes, the
    // staleness reaper fires at 90 seconds — sharing a thread with the
    // heartbeat would mark this host offline in the middle of every real
    // deploy.
    let conn = Arc::new(conn);
    {
        let conn = Arc::clone(&conn);
        let host_name = facts.name.clone();
        std::thread::spawn(move || {
            // Give the subscription a moment to land before the first look.
            std::thread::sleep(Duration::from_secs(2));
            recover_stranded(&conn, &host_name);
            loop {
                for job in queued_for_me(&conn, &host_name) {
                    executor.run(&conn, job.id, &job.app, &job.sha, &job.strategy, &job.target);
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        });
    }

    let mut sys = System::new();
    // The first CPU sample has no previous tick to diff against, so take one
    // and discard it before the loop.
    probe::load(&mut sys);
    std::thread::sleep(Duration::from_millis(300));

    tracing::info!(interval = args.interval, "reporting");
    loop {
        let load = probe::load(&mut sys);
        let workloads = probe::workloads(args.include_system_services);

        if let Err(e) = conn.reducers.heartbeat(
            load.cpu_pct,
            load.mem_pct,
            AGENT_VERSION.to_string(),
            workloads.clone(),
        ) {
            tracing::warn!("heartbeat failed: {e}");
        } else {
            tracing::debug!(
                cpu = load.cpu_pct,
                mem = load.mem_pct,
                workloads = workloads.len(),
                "heartbeat"
            );
        }

        std::thread::sleep(Duration::from_secs(args.interval));
    }
}

/// Fail deployments a previous agent process left in Running.
///
/// They can never finish on their own — the process that was running them is
/// gone — and an honest failure the operator can retry beats an eternal
/// "Running".
fn recover_stranded(conn: &DbConnection, host_name: &str) {
    use module_bindings::{append_log, finish_deployment};

    let db = &conn.db;
    let Some(host) = db.host().iter().find(|h| h.name == host_name) else {
        return;
    };
    let stranded: Vec<u64> = db
        .deployment()
        .iter()
        .filter(|d| d.host_id == host.id && d.status == DeployStatus::Running)
        .map(|d| d.id)
        .collect();

    for id in stranded {
        tracing::warn!(deployment = id, "found a deployment stranded by a previous agent run");
        let _ = conn.reducers.append_log(
            id,
            4_000_000_000,
            "stderr".to_string(),
            "The agent restarted while this deployment was running; marking it failed.".to_string(),
        );
        let _ = conn.reducers.finish_deployment(id, false);
    }
}
