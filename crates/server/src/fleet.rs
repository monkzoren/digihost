//! Turning raw SpacetimeDB rows into something a page can render.
//!
//! The client cache holds the module's tables verbatim. Everything the views
//! need that is not literally a column — relative times, workload labels,
//! status tone, headline counts — is derived here, once, so the templates
//! stay dumb.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use spacetimedb_sdk::Timestamp;

use crate::module_bindings::{
    Application, DeployLogLine, DeployStatus, Deployment, Host, HostStatus, Platform, Workload,
    WorkloadKind,
};

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Ok,
    Info,
    Warn,
    Bad,
    Idle,
}

pub struct HostRow {
    pub id: u64,
    pub name: String,
    pub region: String,
    pub address: String,
    pub platform_slug: &'static str,
    pub runtime: String,
    pub agent_version: String,
    pub cpu_pct: u8,
    pub mem_pct: u8,
    pub workload_label: String,
    pub status: &'static str,
    pub tone: Tone,
}

pub struct DeploymentRow {
    pub id: u64,
    pub app: String,
    pub commit: String,
    pub host: String,
    pub strategy: String,
    pub platform_slug: &'static str,
    pub duration: String,
    pub when: String,
    pub status: &'static str,
    pub tone: Tone,
}

pub struct FleetSnapshot {
    pub hosts: Vec<HostRow>,
    pub deployments: Vec<DeploymentRow>,
    pub total_hosts: usize,
    pub online_hosts: usize,
    pub unreachable_hosts: usize,
    pub linux_hosts: usize,
    pub windows_hosts: usize,
    pub workloads: usize,
    pub deploys_today: usize,
    pub failed: usize,
    pub regions: usize,
}

impl FleetSnapshot {
    /// The one-line subtitle under the page title. Says what is actually
    /// true, including when nothing is enrolled yet.
    pub fn summary(&self) -> String {
        if self.total_hosts == 0 {
            return "No hosts enrolled yet".to_string();
        }
        fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
            if n == 1 {
                one
            } else {
                many
            }
        }
        format!(
            "{} {} across {} {} — {} Linux, {} Windows",
            self.total_hosts,
            plural(self.total_hosts, "host", "hosts"),
            self.regions,
            plural(self.regions, "region", "regions"),
            self.linux_hosts,
            self.windows_hosts
        )
    }
}

/// One line of a deployment's output.
pub struct LogLine {
    pub seq: u32,
    pub stderr: bool,
    pub text: String,
}

/// A deployment plus its streamed log, for the detail drawer.
pub struct DeployDetail {
    pub app: String,
    pub host: String,
    pub commit_sha: String,
    pub commit_message: String,
    pub strategy: String,
    pub status: &'static str,
    pub tone: Tone,
    /// True while the agent is still working, so the view knows to present a
    /// waiting state rather than a finished-empty one.
    pub running: bool,
    pub lines: Vec<LogLine>,
}

pub fn build_detail(
    deployment: &Deployment,
    app: Option<&Application>,
    host: Option<&Host>,
    mut lines: Vec<DeployLogLine>,
) -> DeployDetail {
    let (status, tone) = deploy_state(deployment.status);

    // Agents number their own lines; ordering by seq keeps the log readable
    // even if rows arrive out of order.
    lines.sort_by_key(|l| l.seq);

    DeployDetail {
        app: app
            .map(|a| a.name.clone())
            .unwrap_or_else(|| format!("app {}", deployment.app_id)),
        host: host
            .map(|h| h.name.clone())
            .unwrap_or_else(|| format!("host {}", deployment.host_id)),
        commit_sha: short_sha(&deployment.commit_sha),
        commit_message: deployment.commit_message.clone(),
        strategy: deployment.strategy.clone(),
        status,
        tone,
        running: matches!(deployment.status, DeployStatus::Queued | DeployStatus::Running),
        lines: lines
            .into_iter()
            .map(|l| LogLine {
                seq: l.seq,
                stderr: l.stream == "stderr",
                text: l.text,
            })
            .collect(),
    }
}

fn now_micros() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i128)
        .unwrap_or(0)
}

fn micros(ts: Timestamp) -> i128 {
    ts.to_duration_since_unix_epoch()
        .unwrap_or_default()
        .as_micros() as i128
}

/// "6 min ago", "3 h ago" — coarse on purpose. An operator scanning a fleet
/// wants the magnitude, not the second.
fn relative(then: i128, now: i128) -> String {
    let secs = ((now - then) / 1_000_000).max(0);
    match secs {
        0..=45 => "just now".to_string(),
        46..=5400 => format!("{} min ago", (secs as f64 / 60.0).round() as i64),
        5401..=172_800 => format!("{} h ago", (secs as f64 / 3600.0).round() as i64),
        _ => format!("{} d ago", secs / 86_400),
    }
}

fn duration_between(start: Option<Timestamp>, end: Option<Timestamp>) -> String {
    let (Some(a), Some(b)) = (start, end) else {
        return "—".to_string();
    };
    let secs = ((micros(b) - micros(a)) / 1_000_000).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

pub fn platform_slug(p: Platform) -> &'static str {
    match p {
        Platform::Linux => "linux",
        Platform::Windows => "windows",
    }
}

fn host_state(s: HostStatus) -> (&'static str, Tone) {
    match s {
        HostStatus::Online => ("Online", Tone::Ok),
        HostStatus::Draining => ("Draining", Tone::Info),
        HostStatus::Offline => ("Offline", Tone::Idle),
    }
}

pub fn deploy_state(s: DeployStatus) -> (&'static str, Tone) {
    match s {
        DeployStatus::Queued => ("Queued", Tone::Idle),
        DeployStatus::Running => ("Running", Tone::Info),
        DeployStatus::Succeeded => ("Succeeded", Tone::Ok),
        DeployStatus::Failed => ("Failed", Tone::Bad),
        DeployStatus::RolledBack => ("Rolled back", Tone::Warn),
    }
}

/// "6 containers", "3 IIS sites" — named by whatever kind dominates the host,
/// which reads better at a glance than a bare number.
fn workload_label(kinds: &[WorkloadKind]) -> String {
    if kinds.is_empty() {
        return "none".to_string();
    }
    let mut tally: HashMap<&'static str, usize> = HashMap::new();
    for kind in kinds {
        let noun = match kind {
            WorkloadKind::DockerContainer | WorkloadKind::PodmanContainer => "container",
            WorkloadKind::SystemdUnit => "unit",
            WorkloadKind::IisSite => "IIS site",
            WorkloadKind::WindowsService => "service",
        };
        *tally.entry(noun).or_default() += 1;
    }
    let noun = tally
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(k, _)| *k)
        .unwrap_or("workload");

    let total = kinds.len();
    if total == 1 {
        format!("1 {noun}")
    } else {
        format!("{total} {noun}s")
    }
}

pub fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

pub fn build(
    hosts: Vec<Host>,
    workloads: Vec<Workload>,
    deployments: Vec<Deployment>,
    app_names: HashMap<u64, String>,
) -> FleetSnapshot {
    let now = now_micros();

    let mut by_host: HashMap<u64, Vec<WorkloadKind>> = HashMap::new();
    for w in &workloads {
        by_host.entry(w.host_id).or_default().push(w.kind.clone());
    }

    let region_count = {
        let mut regions: Vec<&str> = hosts.iter().map(|h| h.region.as_str()).collect();
        regions.sort_unstable();
        regions.dedup();
        regions.len()
    };

    let total_hosts = hosts.len();
    let online_hosts = hosts
        .iter()
        .filter(|h| !matches!(h.status, HostStatus::Offline))
        .count();
    let linux_hosts = hosts
        .iter()
        .filter(|h| matches!(h.platform, Platform::Linux))
        .count();

    let host_names: HashMap<u64, (String, &'static str)> = hosts
        .iter()
        .map(|h| (h.id, (h.name.clone(), platform_slug(h.platform))))
        .collect();

    let mut host_rows: Vec<HostRow> = hosts
        .into_iter()
        .map(|h| {
            let (status, tone) = host_state(h.status);
            let kinds = by_host.get(&h.id).cloned().unwrap_or_default();
            HostRow {
                id: h.id,
                name: h.name,
                region: h.region,
                address: h.address,
                platform_slug: platform_slug(h.platform),
                runtime: h.runtime,
                agent_version: h.agent_version,
                cpu_pct: h.cpu_pct,
                mem_pct: h.mem_pct,
                workload_label: workload_label(&kinds),
                status,
                tone,
            }
        })
        .collect();
    host_rows.sort_by(|a, b| a.name.cmp(&b.name));

    let deploys_today = deployments
        .iter()
        .filter(|d| now - micros(d.queued_at) < 86_400_000_000)
        .count();
    let failed = deployments
        .iter()
        .filter(|d| matches!(d.status, DeployStatus::Failed))
        .count();

    let mut deployment_rows: Vec<DeploymentRow> = deployments
        .into_iter()
        .map(|d| {
            let (status, tone) = deploy_state(d.status);
            let (host, slug) = host_names
                .get(&d.host_id)
                .cloned()
                .unwrap_or_else(|| (format!("host {}", d.host_id), "linux"));
            let stamp = d.finished_at.or(d.started_at).unwrap_or(d.queued_at);
            DeploymentRow {
                id: d.id,
                app: app_names
                    .get(&d.app_id)
                    .cloned()
                    .unwrap_or_else(|| format!("app {}", d.app_id)),
                commit: format!("{} · {}", d.commit_message, short_sha(&d.commit_sha)),
                host,
                strategy: d.strategy,
                platform_slug: slug,
                duration: duration_between(d.started_at, d.finished_at),
                when: relative(micros(stamp), now),
                status,
                tone,
            }
        })
        .collect();
    // Newest first: a deploy feed is read from the top.
    deployment_rows.sort_by(|a, b| b.id.cmp(&a.id));

    FleetSnapshot {
        hosts: host_rows,
        deployments: deployment_rows,
        total_hosts,
        online_hosts,
        unreachable_hosts: total_hosts - online_hosts,
        linux_hosts,
        windows_hosts: total_hosts - linux_hosts,
        workloads: workloads.len(),
        deploys_today,
        failed,
        regions: region_count,
    }
}
