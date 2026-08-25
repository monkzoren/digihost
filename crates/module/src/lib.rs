//! DigiHost control plane.
//!
//! The whole control plane lives in the database: hosts, workloads,
//! applications and deployments are tables, and every state transition is a
//! reducer. Agents and DigiHost Server are both just SpacetimeDB clients —
//! there is no API tier for fleet state.
//!
//! Two access rules run through everything here:
//!
//! * **Operator actions are guarded in the reducer**, not only in the web
//!   interface. Agents are clients too, and without `require_operator` any
//!   enrolled host could mint enrolment codes or queue deployments onto its
//!   neighbours.
//! * **No secrets, ever.** SpacetimeDB 2.0.2 ships `client_visibility_filter`
//!   but documents it as unimplemented and unenforced: a public table is
//!   readable by every connected client. Credentials and application
//!   environment therefore live in DigiHost Server's own store and travel
//!   over its authenticated HTTP channel, never through these tables.

use spacetimedb::{
    reducer, table, Identity, ReducerContext, ScheduleAt, SpacetimeType, Table, Timestamp,
};

/// A host is considered unreachable once its agent has been silent this long.
const STALE_AFTER_MICROS: i128 = 90_000_000; // 90s
/// How often the reaper sweeps for silent agents.
const REAP_INTERVAL_MICROS: u64 = 15_000_000; // 15s

// ---------------------------------------------------------------- value types

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq)]
pub enum Platform {
    Linux,
    Windows,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq)]
pub enum HostStatus {
    /// Agent connected and heartbeating.
    Online,
    /// Still reachable, but being taken out of rotation.
    Draining,
    /// Agent silent past the staleness window, or cleanly disconnected.
    Offline,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub enum WorkloadKind {
    DockerContainer,
    PodmanContainer,
    SystemdUnit,
    IisSite,
    WindowsService,
}

/// Where an application's source comes from.
///
/// Coarse on purpose: the agent never acts on this. DigiHost Server does,
/// because it is the only component allowed to hold a credential.
#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq)]
pub enum SourceKind {
    /// Public GitHub repository, fetched without credentials.
    GitHubPublic,
    /// Private repository reached through the instance's GitHub App.
    GitHubApp,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq)]
pub enum DeployStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    RolledBack,
}

/// One workload as observed by an agent during a heartbeat.
#[derive(SpacetimeType, Clone, Debug)]
pub struct WorkloadReport {
    pub name: String,
    pub kind: WorkloadKind,
    pub state: String,
}

// --------------------------------------------------------------------- tables

#[table(accessor = host, public)]
pub struct Host {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[unique]
    pub name: String,
    pub region: String,
    pub address: String,
    pub platform: Platform,
    /// e.g. "Ubuntu 24.04 LTS" / "Windows 11 Home"
    pub os_name: String,
    /// e.g. "Docker 27.3 · systemd" / "IIS · .NET 9 · Windows Services"
    pub runtime: String,
    pub agent_version: String,
    pub status: HostStatus,
    pub cpu_pct: u8,
    pub mem_pct: u8,
    pub workload_count: u32,
    pub last_seen: Timestamp,
    pub enrolled_at: Timestamp,
}

/// Maps an agent's SpacetimeDB identity to the host it speaks for.
/// Private: clients have no business reading agent identities.
#[table(accessor = agent_binding)]
pub struct AgentBinding {
    #[primary_key]
    pub identity: Identity,
    #[unique]
    pub host_id: u64,
    pub bound_at: Timestamp,
}

/// Identities allowed to perform operator actions — in practice, exactly one:
/// DigiHost Server, which claims the instance on first start. Private.
#[table(accessor = operator)]
pub struct Operator {
    #[primary_key]
    pub identity: Identity,
    pub claimed_at: Timestamp,
}

/// Single-use code authorising one agent to enrol as a host. Private.
#[table(accessor = enrollment_token)]
pub struct EnrollmentToken {
    #[primary_key]
    pub code: String,
    pub region: String,
    pub created_at: Timestamp,
    pub consumed_by: Option<u64>,
}

#[table(accessor = workload, public, index(accessor = by_host, btree(columns = [host_id])))]
pub struct Workload {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub host_id: u64,
    pub name: String,
    pub kind: WorkloadKind,
    pub state: String,
}

#[table(accessor = application, public)]
pub struct Application {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[unique]
    pub name: String,
    pub source_kind: SourceKind,
    /// Always "owner/name" — both source kinds are GitHub.
    pub repo: String,
    pub default_branch: String,
    /// What to run, for strategies that install a service, unit or container.
    /// Empty means the target must already exist; DigiHost only deploys into it.
    pub entrypoint: String,
    /// Port to bind for HTTP-facing strategies. 0 means unset.
    pub port: u16,
    /// Where releases are installed on a host. Empty means the strategy's
    /// conventional location.
    pub deploy_path: String,
    /// How this application normally deploys. A deployment may override it,
    /// but this is what the interface offers first.
    pub default_strategy: String,
}

#[table(
    accessor = deployment,
    public,
    index(accessor = by_host, btree(columns = [host_id])),
    index(accessor = by_app, btree(columns = [app_id]))
)]
pub struct Deployment {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub app_id: u64,
    pub host_id: u64,
    pub commit_sha: String,
    pub commit_message: String,
    /// How the agent applies it: "Static files", "Dockerfile", "Docker
    /// Compose", "systemd unit", "IIS site swap", "Windows Service".
    pub strategy: String,
    pub status: DeployStatus,
    pub queued_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

#[table(
    accessor = deploy_log_line,
    public,
    index(accessor = by_deployment, btree(columns = [deployment_id]))
)]
pub struct DeployLogLine {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub deployment_id: u64,
    pub seq: u32,
    /// "stdout" | "stderr"
    pub stream: String,
    pub text: String,
    pub at: Timestamp,
}

#[table(accessor = reaper_schedule, scheduled(reap_stale_hosts))]
pub struct ReaperSchedule {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

// ------------------------------------------------------------------ lifecycle

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    ctx.db.reaper_schedule().insert(ReaperSchedule {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Interval(
            std::time::Duration::from_micros(REAP_INTERVAL_MICROS).into(),
        ),
    });
    log::info!("digihost control plane initialised");
}

#[reducer(client_connected)]
pub fn client_connected(ctx: &ReducerContext) {
    // Only agents have a binding; the server and any stray client have none.
    let Some(binding) = ctx.db.agent_binding().identity().find(ctx.sender()) else {
        return;
    };
    if let Some(host) = ctx.db.host().id().find(&binding.host_id) {
        let name = host.name.clone();
        ctx.db.host().id().update(Host {
            status: HostStatus::Online,
            last_seen: ctx.timestamp,
            ..host
        });
        log::info!("agent for {name} connected");
    }
}

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    let Some(binding) = ctx.db.agent_binding().identity().find(ctx.sender()) else {
        return;
    };
    if let Some(host) = ctx.db.host().id().find(&binding.host_id) {
        let name = host.name.clone();
        ctx.db.host().id().update(Host {
            status: HostStatus::Offline,
            ..host
        });
        log::warn!("agent for {name} disconnected");
    }
}

/// Scheduled sweep: an agent that died without disconnecting cleanly still
/// shows as unreachable within the staleness window.
#[reducer]
pub fn reap_stale_hosts(ctx: &ReducerContext, _schedule: ReaperSchedule) {
    let now = micros(ctx.timestamp);
    for host in ctx.db.host().iter() {
        if host.status == HostStatus::Offline {
            continue;
        }
        if now.saturating_sub(micros(host.last_seen)) > STALE_AFTER_MICROS {
            let name = host.name.clone();
            ctx.db.host().id().update(Host {
                status: HostStatus::Offline,
                ..host
            });
            log::warn!("host {name} went silent; marked offline");
        }
    }
}

// -------------------------------------------------------------- operator side

/// Claim this instance for the calling identity. First caller wins, and only
/// DigiHost Server ever calls it; idempotent because the server calls it on
/// every start.
#[reducer]
pub fn claim_instance(ctx: &ReducerContext) -> Result<(), String> {
    if ctx.db.operator().identity().find(ctx.sender()).is_some() {
        return Ok(());
    }
    if ctx.db.operator().iter().count() > 0 {
        return Err("this instance is already claimed by another operator".to_string());
    }
    ctx.db.operator().insert(Operator {
        identity: ctx.sender(),
        claimed_at: ctx.timestamp,
    });
    log::info!("instance claimed");
    Ok(())
}

fn require_operator(ctx: &ReducerContext) -> Result<(), String> {
    if ctx.db.operator().identity().find(ctx.sender()).is_some() {
        Ok(())
    } else {
        Err("operator action attempted by a non-operator client".to_string())
    }
}

/// Mint a single-use enrolment code for a new host.
#[reducer]
pub fn create_enrollment_token(
    ctx: &ReducerContext,
    code: String,
    region: String,
) -> Result<(), String> {
    require_operator(ctx)?;
    if code.trim().is_empty() {
        return Err("enrolment code cannot be empty".to_string());
    }
    if ctx.db.enrollment_token().code().find(&code).is_some() {
        return Err(format!("enrolment code {code} already exists"));
    }
    ctx.db.enrollment_token().insert(EnrollmentToken {
        code,
        region,
        created_at: ctx.timestamp,
        consumed_by: None,
    });
    Ok(())
}

#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn register_application(
    ctx: &ReducerContext,
    name: String,
    source_kind: SourceKind,
    repo: String,
    default_branch: String,
    entrypoint: String,
    port: u16,
    deploy_path: String,
    default_strategy: String,
) -> Result<(), String> {
    require_operator(ctx)?;
    let name = name.trim().to_string();
    // Names feed compose project names, service names and paths, all of which
    // want a slug: lowercase letters, digits, hyphens, no edge hyphens.
    let name_ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !name_ok {
        return Err(format!(
            "application names are lowercase letters, digits and hyphens — got {name:?}"
        ));
    }
    // Both source kinds are GitHub, addressed as owner/name; anything else
    // gives the server nothing to build an API call from.
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.trim().is_empty()) {
        return Err(format!("expected a GitHub repository as owner/name, got {repo}"));
    }
    if ctx.db.application().name().find(&name).is_some() {
        return Err(format!("application {name} already registered"));
    }
    ctx.db.application().insert(Application {
        id: 0,
        name,
        source_kind,
        repo,
        default_branch,
        entrypoint,
        port,
        deploy_path,
        default_strategy,
    });
    Ok(())
}

/// Take a host out of rotation without disconnecting its agent — or put it
/// back. Never resurrects an offline host.
#[reducer]
pub fn set_host_draining(ctx: &ReducerContext, host_id: u64, draining: bool) -> Result<(), String> {
    require_operator(ctx)?;
    let host = ctx
        .db
        .host()
        .id()
        .find(&host_id)
        .ok_or_else(|| format!("no host {host_id}"))?;

    let status = match (draining, host.status) {
        (true, _) => HostStatus::Draining,
        (false, HostStatus::Offline) => HostStatus::Offline,
        (false, _) => HostStatus::Online,
    };
    ctx.db.host().id().update(Host { status, ..host });
    Ok(())
}

#[reducer]
pub fn queue_deployment(
    ctx: &ReducerContext,
    app_id: u64,
    host_id: u64,
    commit_sha: String,
    commit_message: String,
    strategy: String,
) -> Result<(), String> {
    require_operator(ctx)?;
    if ctx.db.application().id().find(&app_id).is_none() {
        return Err(format!("no application {app_id}"));
    }
    let host = ctx
        .db
        .host()
        .id()
        .find(&host_id)
        .ok_or_else(|| format!("no host {host_id}"))?;
    match host.status {
        HostStatus::Online => {}
        HostStatus::Draining => return Err(format!("{} is draining", host.name)),
        HostStatus::Offline => return Err(format!("{} is unreachable", host.name)),
    }

    ctx.db.deployment().insert(Deployment {
        id: 0,
        app_id,
        host_id,
        commit_sha,
        commit_message,
        strategy,
        status: DeployStatus::Queued,
        queued_at: ctx.timestamp,
        started_at: None,
        finished_at: None,
    });
    Ok(())
}

/// Mark a succeeded deployment as rolled back. DigiHost Server pairs this
/// with queueing the previous release — the record and the redeploy are two
/// steps of one operator action.
#[reducer]
pub fn rollback_deployment(ctx: &ReducerContext, deployment_id: u64) -> Result<(), String> {
    require_operator(ctx)?;
    let dep = ctx
        .db
        .deployment()
        .id()
        .find(&deployment_id)
        .ok_or_else(|| format!("no deployment {deployment_id}"))?;
    if dep.status != DeployStatus::Succeeded {
        return Err("only a succeeded deployment can be rolled back".to_string());
    }
    ctx.db.deployment().id().update(Deployment {
        status: DeployStatus::RolledBack,
        ..dep
    });
    Ok(())
}

/// Remove an application. Its deployment history stays (the interface falls
/// back to "app N" for those rows), and nothing running on any host is
/// touched — removing the record is not an order to tear workloads down.
#[reducer]
pub fn delete_application(ctx: &ReducerContext, app_id: u64) -> Result<(), String> {
    require_operator(ctx)?;
    let app = ctx
        .db
        .application()
        .id()
        .find(&app_id)
        .ok_or_else(|| format!("no application {app_id}"))?;
    let busy = ctx
        .db
        .deployment()
        .by_app()
        .filter(&app_id)
        .any(|d| matches!(d.status, DeployStatus::Queued | DeployStatus::Running));
    if busy {
        return Err(format!("{} has a deployment in flight", app.name));
    }
    ctx.db.application().id().delete(&app_id);
    log::info!("application {} removed", app.name);
    Ok(())
}

/// Decommission a host. Refused while its agent is still connected — stop the
/// agent first — so a live machine cannot be silently dropped from view.
#[reducer]
pub fn delete_host(ctx: &ReducerContext, host_id: u64) -> Result<(), String> {
    require_operator(ctx)?;
    let host = ctx
        .db
        .host()
        .id()
        .find(&host_id)
        .ok_or_else(|| format!("no host {host_id}"))?;
    if host.status != HostStatus::Offline {
        return Err(format!(
            "{} is still reporting — stop its agent, wait for it to show Offline, then remove it",
            host.name
        ));
    }

    for w in ctx.db.workload().by_host().filter(&host_id).collect::<Vec<_>>() {
        ctx.db.workload().id().delete(&w.id);
    }
    // Freeing the binding lets the same machine re-enrol under the same name
    // with a fresh code later.
    if let Some(binding) = ctx.db.agent_binding().host_id().find(&host_id) {
        ctx.db.agent_binding().identity().delete(&binding.identity);
    }
    let name = host.name.clone();
    ctx.db.host().id().delete(&host_id);
    log::info!("host {name} decommissioned");
    Ok(())
}

// ----------------------------------------------------------------- agent side

/// First contact from an agent: consumes an enrolment code and binds the
/// caller's identity to a freshly created host row.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn enroll_host(
    ctx: &ReducerContext,
    code: String,
    name: String,
    address: String,
    platform: Platform,
    os_name: String,
    runtime: String,
    agent_version: String,
) -> Result<(), String> {
    let token = ctx
        .db
        .enrollment_token()
        .code()
        .find(&code)
        .ok_or("unknown enrolment code")?;
    if token.consumed_by.is_some() {
        return Err("enrolment code already used".to_string());
    }
    if ctx.db.agent_binding().identity().find(ctx.sender()).is_some() {
        return Err("this agent identity is already enrolled".to_string());
    }
    if ctx.db.host().name().find(&name).is_some() {
        return Err(format!("host {name} already enrolled"));
    }

    let region = token.region.clone();
    let host = ctx.db.host().insert(Host {
        id: 0,
        name: name.clone(),
        region,
        address,
        platform,
        os_name,
        runtime,
        agent_version,
        status: HostStatus::Online,
        cpu_pct: 0,
        mem_pct: 0,
        workload_count: 0,
        last_seen: ctx.timestamp,
        enrolled_at: ctx.timestamp,
    });

    ctx.db.agent_binding().insert(AgentBinding {
        identity: ctx.sender(),
        host_id: host.id,
        bound_at: ctx.timestamp,
    });
    ctx.db.enrollment_token().code().update(EnrollmentToken {
        consumed_by: Some(host.id),
        ..token
    });

    log::info!("host {name} enrolled as id {}", host.id);
    Ok(())
}

/// Resolve the host the calling agent speaks for, or refuse. Every agent-side
/// reducer goes through this, so an agent can only ever touch its own host.
fn caller_host(ctx: &ReducerContext) -> Result<Host, String> {
    let binding = ctx
        .db
        .agent_binding()
        .identity()
        .find(ctx.sender())
        .ok_or("caller is not an enrolled agent")?;
    ctx.db
        .host()
        .id()
        .find(&binding.host_id)
        .ok_or_else(|| format!("binding points at missing host {}", binding.host_id))
}

/// Periodic agent report: resource load plus the full current workload set.
#[reducer]
pub fn heartbeat(
    ctx: &ReducerContext,
    cpu_pct: u8,
    mem_pct: u8,
    agent_version: String,
    workloads: Vec<WorkloadReport>,
) -> Result<(), String> {
    let host = caller_host(ctx)?;

    // The report is authoritative: replace this host's workload set wholesale
    // so vanished containers and services actually disappear.
    for existing in ctx.db.workload().by_host().filter(&host.id) {
        ctx.db.workload().id().delete(&existing.id);
    }
    for w in &workloads {
        ctx.db.workload().insert(Workload {
            id: 0,
            host_id: host.id,
            name: w.name.clone(),
            kind: w.kind.clone(),
            state: w.state.clone(),
        });
    }

    // Draining is sticky: a heartbeat must not silently undo an operator's
    // decision to take the host out of rotation.
    let status = match host.status {
        HostStatus::Draining => HostStatus::Draining,
        _ => HostStatus::Online,
    };

    ctx.db.host().id().update(Host {
        cpu_pct: cpu_pct.min(100),
        mem_pct: mem_pct.min(100),
        workload_count: workloads.len() as u32,
        agent_version,
        status,
        last_seen: ctx.timestamp,
        ..host
    });
    Ok(())
}

/// Agent claims a queued deployment targeting its own host.
#[reducer]
pub fn start_deployment(ctx: &ReducerContext, deployment_id: u64) -> Result<(), String> {
    let host = caller_host(ctx)?;
    let dep = owned_deployment(ctx, &host, deployment_id)?;
    if dep.status != DeployStatus::Queued {
        return Err("deployment is not queued".to_string());
    }
    ctx.db.deployment().id().update(Deployment {
        status: DeployStatus::Running,
        started_at: Some(ctx.timestamp),
        ..dep
    });
    Ok(())
}

/// Stream one line of build/deploy output.
#[reducer]
pub fn append_log(
    ctx: &ReducerContext,
    deployment_id: u64,
    seq: u32,
    stream: String,
    text: String,
) -> Result<(), String> {
    let host = caller_host(ctx)?;
    owned_deployment(ctx, &host, deployment_id)?;
    ctx.db.deploy_log_line().insert(DeployLogLine {
        id: 0,
        deployment_id,
        seq,
        stream,
        text,
        at: ctx.timestamp,
    });
    Ok(())
}

#[reducer]
pub fn finish_deployment(
    ctx: &ReducerContext,
    deployment_id: u64,
    succeeded: bool,
) -> Result<(), String> {
    let host = caller_host(ctx)?;
    let dep = owned_deployment(ctx, &host, deployment_id)?;
    let status = if succeeded {
        DeployStatus::Succeeded
    } else {
        DeployStatus::Failed
    };
    ctx.db.deployment().id().update(Deployment {
        status,
        finished_at: Some(ctx.timestamp),
        ..dep
    });
    Ok(())
}

fn owned_deployment(
    ctx: &ReducerContext,
    host: &Host,
    deployment_id: u64,
) -> Result<Deployment, String> {
    let dep = ctx
        .db
        .deployment()
        .id()
        .find(&deployment_id)
        .ok_or_else(|| format!("no deployment {deployment_id}"))?;
    if dep.host_id != host.id {
        return Err("deployment targets a different host".to_string());
    }
    Ok(dep)
}

// --------------------------------------------------------------------- shared

fn micros(ts: Timestamp) -> i128 {
    ts.to_duration_since_unix_epoch()
        .unwrap_or_default()
        .as_micros() as i128
}
