//! Server-rendered views.
//!
//! The server owns rendering: browsers receive finished markup, both on first
//! load and on every SSE push. That keeps one implementation of "what a host
//! row looks like" instead of one in Rust and one in JavaScript.

use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::fleet::{DeployDetail, DeploymentRow, FleetSnapshot, HostRow, Tone};
use crate::store::EnvVar;

const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

/// One application as the Applications page shows it.
pub struct AppOverview {
    pub id: u64,
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub private: bool,
    pub strategy: String,
    pub entrypoint: String,
    pub port: u16,
    pub deploy_path: String,
    pub env_count: usize,
    pub last_status: &'static str,
    pub last_tone: Tone,
}

/// Stroke icons on a 24px grid. Emoji would not scale or recolour, and the
/// platform glyphs deliberately avoid vendor marks: a terminal for Linux, a
/// titled window for Windows, each always paired with its label.
pub mod icon {
    use maud::{html, Markup, PreEscaped};

    fn svg(size: u32, body: &str) -> Markup {
        html! {
            svg width=(size) height=(size) viewBox="0 0 24 24" fill="none"
                stroke="currentColor" stroke-width="1.75"
                stroke-linecap="round" stroke-linejoin="round" {
                (PreEscaped(body.to_string()))
            }
        }
    }

    pub fn servers(size: u32) -> Markup {
        svg(size, r#"<rect x="3" y="4" width="18" height="7" rx="2"></rect><rect x="3" y="13" width="18" height="7" rx="2"></rect><path d="M7 7.5h.01M7 16.5h.01"></path>"#)
    }
    pub fn grid(size: u32) -> Markup {
        svg(size, r#"<rect x="3" y="3" width="7" height="7" rx="1.5"></rect><rect x="14" y="3" width="7" height="7" rx="1.5"></rect><rect x="3" y="14" width="7" height="7" rx="1.5"></rect><rect x="14" y="14" width="7" height="7" rx="1.5"></rect>"#)
    }
    pub fn chip(size: u32) -> Markup {
        svg(size, r#"<rect x="6" y="6" width="12" height="12" rx="2"></rect><path d="M10 2v4M14 2v4M10 18v4M14 18v4M2 10h4M2 14h4M18 10h4M18 14h4"></path>"#)
    }
    pub fn network(size: u32) -> Markup {
        svg(size, r#"<circle cx="12" cy="5" r="2.5"></circle><circle cx="5" cy="19" r="2.5"></circle><circle cx="19" cy="19" r="2.5"></circle><path d="M12 7.5v3M12 10.5 6.5 16.9M12 10.5l5.5 6.4"></path>"#)
    }
    pub fn deploy(size: u32) -> Markup {
        svg(size, r#"<path d="M12 19V7"></path><path d="m6 12 6-6 6 6"></path><path d="M5 21h14"></path>"#)
    }
    pub fn package(size: u32) -> Markup {
        svg(size, r#"<path d="M21 8 12 3 3 8v8l9 5 9-5z"></path><path d="m3 8 9 5 9-5"></path><path d="M12 13v8"></path>"#)
    }
    pub fn branch(size: u32) -> Markup {
        svg(size, r#"<circle cx="7" cy="5" r="2"></circle><circle cx="7" cy="19" r="2"></circle><circle cx="17" cy="9" r="2"></circle><path d="M7 7v10"></path><path d="M17 11c0 3.5-4 3-6 5"></path>"#)
    }
    pub fn archive(size: u32) -> Markup {
        svg(size, r#"<rect x="3" y="4" width="18" height="4" rx="1"></rect><path d="M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8"></path><path d="M10 12h4"></path>"#)
    }
    pub fn database(size: u32) -> Markup {
        svg(size, r#"<ellipse cx="12" cy="6" rx="8" ry="3"></ellipse><path d="M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6"></path><path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"></path>"#)
    }
    pub fn save(size: u32) -> Markup {
        svg(size, r#"<path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z"></path><path d="M17 21v-8H7v8"></path><path d="M7 3v5h8"></path>"#)
    }
    pub fn disk(size: u32) -> Markup {
        svg(size, r#"<rect x="3" y="4" width="18" height="16" rx="2"></rect><path d="M3 12h18"></path><path d="M7 16h.01M11 16h.01"></path>"#)
    }
    pub fn activity(size: u32) -> Markup {
        svg(size, r#"<path d="M3 12h4l3 8 4-16 3 8h4"></path>"#)
    }
    pub fn logs(size: u32) -> Markup {
        svg(size, r#"<path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01"></path>"#)
    }
    pub fn chart(size: u32) -> Markup {
        svg(size, r#"<path d="M3 21h18"></path><rect x="5" y="11" width="4" height="7" rx="1"></rect><rect x="11" y="6" width="4" height="12" rx="1"></rect><rect x="17" y="14" width="4" height="4" rx="1"></rect>"#)
    }
    pub fn bell(size: u32) -> Markup {
        svg(size, r#"<path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"></path><path d="M10.3 21a2 2 0 0 0 3.4 0"></path>"#)
    }
    pub fn sliders(size: u32) -> Markup {
        svg(size, r#"<path d="M4 6h16M4 12h16M4 18h16"></path><circle cx="9" cy="6" r="2"></circle><circle cx="15" cy="12" r="2"></circle><circle cx="8" cy="18" r="2"></circle>"#)
    }
    pub fn users(size: u32) -> Markup {
        svg(size, r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"></path><circle cx="9" cy="7" r="4"></circle><path d="M22 21v-2a4 4 0 0 0-3-3.9"></path>"#)
    }
    pub fn key(size: u32) -> Markup {
        svg(size, r#"<circle cx="7.5" cy="15.5" r="3.5"></circle><path d="m10 13 8-8"></path><path d="m15 8 2 2"></path><path d="m18 5 2 2"></path>"#)
    }
    pub fn plus(size: u32) -> Markup {
        svg(size, r#"<path d="M12 5v14M5 12h14"></path>"#)
    }
    pub fn cross(size: u32) -> Markup {
        svg(size, r#"<circle cx="12" cy="12" r="9"></circle><path d="m15 9-6 6M9 9l6 6"></path>"#)
    }
    pub fn linux(size: u32) -> Markup {
        svg(size, r#"<rect x="3" y="4" width="18" height="16" rx="2"></rect><path d="m7 9 3 3-3 3M13 15h4"></path>"#)
    }
    pub fn windows(size: u32) -> Markup {
        svg(size, r#"<rect x="3" y="4" width="18" height="16" rx="2"></rect><path d="M3 9h18"></path><path d="M6.5 6.5h.01M9.5 6.5h.01"></path>"#)
    }
    pub fn chevron(size: u32) -> Markup {
        html! {
            svg class="chev" width=(size) height=(size) viewBox="0 0 24 24" fill="none"
                stroke="currentColor" stroke-width="2"
                stroke-linecap="round" stroke-linejoin="round" {
                (PreEscaped(r#"<path d="m9 6 6 6-6 6"></path>"#.to_string()))
            }
        }
    }
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Ok => "ok",
        Tone::Info => "info",
        Tone::Warn => "warn",
        Tone::Bad => "bad",
        Tone::Idle => "idle",
    }
}

/// Green under 60, amber to 80, red above — and slate at zero, so an
/// unreachable host does not read as "healthy and idle".
fn meter_colour(pct: u8) -> &'static str {
    match pct {
        0 => "#475569",
        1..=59 => "#10b981",
        60..=79 => "#f59e0b",
        _ => "#ef4444",
    }
}

fn meter(pct: u8) -> Markup {
    html! {
        div class="meter-val" { (pct) "%" }
        div class="meter" {
            span style=(format!("width:{}%;background:{}", pct, meter_colour(pct))) {}
        }
    }
}

fn platform_badge(is_linux: bool) -> Markup {
    html! {
        @if is_linux {
            span class="os linux" { (icon::linux(12)) "Linux" }
        } @else {
            span class="os windows" { (icon::windows(12)) "Windows" }
        }
    }
}

fn platform_glyph(is_linux: bool) -> Markup {
    html! {
        @if is_linux {
            span style="color:var(--linux);display:flex" { (icon::linux(14)) }
        } @else {
            span style="color:var(--windows);display:flex" { (icon::windows(14)) }
        }
    }
}

fn host_row(host: &HostRow) -> Markup {
    html! {
        div class="tr trow" data-id=(host.id) data-platform=(host.platform_slug) {
            div class="c-host" {
                div class="cell-title" { (host.name) }
                div class="cell-sub mono" { (host.region) " · " (host.address) }
            }
            div class="c-platform" {
                (platform_badge(host.platform_slug == "linux"))
                div class="os-runtime" { (host.runtime) " · agent v" (host.agent_version) }
            }
            div class="c-meter" { (meter(host.cpu_pct)) }
            div class="c-meter" { (meter(host.mem_pct)) }
            div class="c-work" { (host.workload_label) }
            div class="c-status" {
                span class=(format!("pill {}", tone_class(host.tone))) { (host.status) }
            }
            div class="c-actions" {
                @if host.status == "Offline" {
                    button class="ghost" data-action="/actions/delete-host"
                        data-field-host_id=(host.id)
                        data-confirm=(format!("Remove {}? Its agent is offline; the machine itself is not touched.", host.name)) {
                        "Remove"
                    }
                } @else {
                    button class="ghost" data-action="/actions/drain"
                        data-field-host_id=(host.id)
                        data-field-draining=(if host.status == "Draining" { "false" } else { "true" }) {
                        @if host.status == "Draining" { "Resume" } @else { "Drain" }
                    }
                }
            }
        }
    }
}

fn deployment_row(dep: &DeploymentRow) -> Markup {
    html! {
        div class="tr trow" data-id=(format!("d{}", dep.id)) data-platform=(dep.platform_slug) {
            div class="c-host" {
                div class="cell-title" { (dep.app) }
                div class="cell-sub" { (dep.commit) }
            }
            div class="c-target" {
                (platform_glyph(dep.platform_slug == "linux"))
                div style="min-width:0" {
                    div class="cell-title mono" style="font-size:13px;font-weight:400;color:var(--ink-1)" { (dep.host) }
                    div class="cell-sub" { (dep.strategy) }
                }
            }
            div class="c-dur" { (dep.duration) }
            div class="c-when" { (dep.when) }
            div class="c-result" {
                span class=(format!("pill {}", tone_class(dep.tone))) { (dep.status) }
            }
            div class="c-actions" {
                @if dep.status == "Succeeded" {
                    button class="ghost" data-action="/actions/rollback"
                        data-field-deployment_id=(dep.id) { "Roll back" }
                }
            }
        }
    }
}

/// The part of the fleet page that changes when the fleet changes. Rendered
/// on first load and pushed verbatim over SSE thereafter.
pub fn fleet_body(snap: &FleetSnapshot) -> Markup {
    let circumference = 226.19_f64;
    let fraction = if snap.total_hosts == 0 {
        0.0
    } else {
        snap.online_hosts as f64 / snap.total_hosts as f64
    };
    let offset = circumference * (1.0 - fraction);

    html! {
        div class="stats" {
            div class="stat panel" {
                div class="ring" {
                    svg width="80" height="80" viewBox="0 0 80 80" {
                        circle cx="40" cy="40" r="36" fill="none" stroke="#334155" stroke-width="8" {}
                        circle cx="40" cy="40" r="36" fill="none"
                            stroke=(if fraction >= 0.5 { "#34d399" } else { "#fbbf24" })
                            stroke-width="8" stroke-linecap="round"
                            stroke-dasharray=(format!("{circumference:.2}"))
                            stroke-dashoffset=(format!("{offset:.2}")) {}
                    }
                    div class="ring-centre" {
                        span class="ring-num" { (snap.online_hosts) }
                        span class="ring-den" { "/" (snap.total_hosts) }
                    }
                }
                div {
                    p class="stat-label" { "Hosts" }
                    p class="stat-value sm" { (snap.online_hosts) " online" }
                    @if snap.unreachable_hosts > 0 {
                        p class="stat-note" { (snap.unreachable_hosts) " unreachable" }
                    } @else {
                        p class="stat-note" { "all reachable" }
                    }
                }
            }

            div class="stat panel" {
                div class="stat-chip" style="background:rgba(59,130,246,0.2);color:#60a5fa" { (icon::package(24)) }
                div {
                    p class="stat-label" { "Workloads" }
                    p class="stat-value" { (snap.workloads) }
                }
            }

            div class="stat panel" {
                div class="stat-chip" style="background:rgba(16,185,129,0.2);color:#34d399" { (icon::deploy(24)) }
                div {
                    p class="stat-label" { "Deploys today" }
                    p class="stat-value" { (snap.deploys_today) }
                }
            }

            div class="stat panel" {
                div class="stat-chip" style="background:rgba(239,68,68,0.2);color:#f87171" { (icon::cross(24)) }
                div {
                    p class="stat-label" { "Failed" }
                    p class="stat-value" { (snap.failed) }
                }
            }
        }

        div class="tabbar" {
            div class="tabs" {
                button class="tab active" data-tab="servers" { "Servers" }
                button class="tab" data-tab="deployments" { "Deployments" }
            }
            div class="filters" {
                span class="filters-label" { "Platform" }
                div class="chips" {
                    button class="chip active" data-platform="all" { "All " span data-count { "0" } }
                    button class="chip" data-platform="linux" { (icon::linux(13)) "Linux " span data-count { "0" } }
                    button class="chip" data-platform="windows" { (icon::windows(13)) "Windows " span data-count { "0" } }
                }
            }
        }

        div class="panel table" data-panel="servers" {
            div class="tr thead" {
                div class="c-host" { "Host" }
                div class="c-platform" { "Platform" }
                div class="c-meter" { "CPU" }
                div class="c-meter" { "Memory" }
                div class="c-work" { "Workloads" }
                div class="c-status" { "Status" }
                div class="c-actions" {}
            }
            @for host in &snap.hosts { (host_row(host)) }
            div class=(if snap.hosts.is_empty() { "empty" } else { "empty hidden" }) {
                p class="empty-title" { "No hosts enrolled yet" }
                p class="empty-body" {
                    "Mint an enrolment code with Add server, then run "
                    code { "digihost-agent --enrollment-code <code>" }
                    " on the machine you want to manage."
                }
            }
        }

        div class="panel table hidden" data-panel="deployments" {
            div class="tr thead" {
                div class="c-host" { "Application" }
                div class="c-target" { "Target" }
                div class="c-dur" { "Duration" }
                div class="c-when" { "Finished" }
                div class="c-result" { "Result" }
                div class="c-actions" {}
            }
            @for dep in &snap.deployments { (deployment_row(dep)) }
            div class=(if snap.deployments.is_empty() { "empty" } else { "empty hidden" }) {
                p class="empty-title" { "Nothing deployed yet" }
                p class="empty-body" { "Deployments appear here as soon as one is queued against a host." }
            }
        }
    }
}

/// The deployment drawer: what was deployed, where, and everything the agent
/// printed while doing it.
pub fn deploy_log(detail: &DeployDetail) -> Markup {
    html! {
        div class="log-head" {
            div {
                div class="log-title" { (detail.app) }
                div class="log-sub" {
                    (detail.commit_message) " · " span class="mono" { (detail.commit_sha) }
                }
                div class="log-sub" { (detail.strategy) " on " span class="mono" { (detail.host) } }
            }
            div style="display:flex;align-items:center;gap:12px" {
                span class=(format!("pill {}", tone_class(detail.tone))) { (detail.status) }
                button class="ghost" data-close-drawer { "Close" }
            }
        }

        @if detail.lines.is_empty() {
            div class="empty" {
                p class="empty-title" {
                    @if detail.running { "Waiting for the agent" } @else { "No output" }
                }
                p class="empty-body" {
                    @if detail.running {
                        "This deployment is queued. Output appears here as the agent works."
                    } @else {
                        "This deployment finished without printing anything."
                    }
                }
            }
        } @else {
            div class="log" {
                @for line in &detail.lines {
                    div class=(if line.stderr { "log-line err" } else { "log-line" }) {
                        span class="log-seq" { (line.seq) }
                        span class="log-text" { (line.text) }
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------------- chrome

struct NavItem {
    id: &'static str,
    label: &'static str,
    href: Option<&'static str>,
    icon: fn(u32) -> Markup,
}

struct NavGroup {
    label: &'static str,
    colour: &'static str,
    icon: fn(u32) -> Markup,
    items: &'static [NavItem],
}

/// Only Fleet and Applications lead anywhere yet; the rest are visible as a
/// roadmap and marked so, rather than silently doing nothing.
const NAV: &[NavGroup] = &[
    NavGroup {
        label: "Infrastructure",
        colour: "#34d399",
        icon: icon::servers,
        items: &[
            NavItem { id: "fleet", label: "Fleet", href: Some("/"), icon: icon::grid },
            NavItem { id: "servers", label: "Servers", href: None, icon: icon::servers },
            NavItem { id: "agents", label: "Agents", href: None, icon: icon::chip },
            NavItem { id: "networks", label: "Private networks", href: None, icon: icon::network },
        ],
    },
    NavGroup {
        label: "Deployments",
        colour: "#60a5fa",
        icon: icon::deploy,
        items: &[
            NavItem { id: "applications", label: "Applications", href: Some("/applications"), icon: icon::package },
            NavItem { id: "pipelines", label: "Pipelines", href: None, icon: icon::branch },
            NavItem { id: "registries", label: "Registries", href: None, icon: icon::archive },
        ],
    },
    NavGroup {
        label: "Data",
        colour: "#c084fc",
        icon: icon::database,
        items: &[
            NavItem { id: "databases", label: "Databases", href: None, icon: icon::database },
            NavItem { id: "backups", label: "Backups", href: None, icon: icon::save },
            NavItem { id: "volumes", label: "Volumes", href: None, icon: icon::disk },
        ],
    },
    NavGroup {
        label: "Observability",
        colour: "#22d3ee",
        icon: icon::activity,
        items: &[
            NavItem { id: "logs", label: "Logs", href: None, icon: icon::logs },
            NavItem { id: "metrics", label: "Metrics", href: None, icon: icon::chart },
            NavItem { id: "alerts", label: "Alerts", href: None, icon: icon::bell },
        ],
    },
    NavGroup {
        label: "Settings",
        colour: "#fb923c",
        icon: icon::sliders,
        items: &[
            NavItem { id: "team", label: "Team", href: Some("/settings/team"), icon: icon::users },
            NavItem { id: "tokens", label: "API tokens", href: Some("/settings/tokens"), icon: icon::key },
        ],
    },
];

fn nav(active: &str) -> Markup {
    html! {
        nav class="nav" {
            @for group in NAV {
                @let open = group.items.iter().any(|i| i.id == active);
                div class=(if open { "nav-group open" } else { "nav-group" }) {
                    button class="nav-section"
                        style=(format!("color:{c};border-left-color:{c}4d", c = group.colour)) {
                        span { ((group.icon)(14)) (group.label) }
                        (icon::chevron(14))
                    }
                    div class="nav-items" {
                        @for item in group.items {
                            @match item.href {
                                Some(href) => {
                                    a class=(if item.id == active { "nav-item active" } else { "nav-item" })
                                        href=(href) {
                                        ((item.icon)(16)) span class="grow" { (item.label) }
                                    }
                                }
                                None => {
                                    a class="nav-item soon" title="Not built yet" {
                                        ((item.icon)(16)) span class="grow" { (item.label) }
                                        span class="soon-tag" { "soon" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn sidebar(
    snap: &FleetSnapshot,
    instance: &str,
    github_ok: bool,
    active: &str,
    version: &str,
    update: Option<&str>,
    user: &str,
) -> Markup {
    html! {
        aside class="sidebar" {
            div class="brand" {
                span style="color:var(--accent);display:flex" { (icon::servers(28)) }
                span class="brand-name" { "DigiHost" }
            }
            div class="whoami" {
                p class="whoami-name" { (instance) }
                p class="whoami-sub" {
                    (snap.total_hosts)
                    (if snap.total_hosts == 1 { " host · " } else { " hosts · " })
                    (snap.regions)
                    (if snap.regions == 1 { " region" } else { " regions" })
                }
            }
            (nav(active))
            div class="sidebar-foot" {
                button class="foot-btn" data-open="dlg-app" {
                    (icon::package(16)) span { "Register application" }
                }
                button class="foot-btn" data-open="dlg-env" {
                    (icon::key(16)) span { "Environment" }
                }
                button class="foot-btn" data-open="dlg-github" {
                    (icon::branch(16))
                    span {
                        @if github_ok { "GitHub App connected" } @else { "Connect GitHub App" }
                    }
                }
                button class="foot-btn" data-action="/logout" data-navigate="/login" {
                    (icon::key(16)) span { "Sign out" }
                }
                @if let Some(latest) = update {
                    a class="update-note" href="https://github.com/monkzoren/digihost/releases"
                        target="_blank" rel="noopener" {
                        "v" (latest) " available — run digihost-update"
                    }
                }
                p class="version" { (user) " · DigiHost v" (version) }
            }
        }
    }
}

fn head(title: &str) -> Markup {
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (title) }
            link rel="preconnect" href="https://fonts.googleapis.com";
            link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
            link rel="stylesheet"
                href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=Oswald:wght@500;600;700&display=swap";
            style { (PreEscaped(APP_CSS)) }
        }
    }
}

// ------------------------------------------------------------------- dialogs

fn dialogs(
    apps: &[(u64, String, String)],
    hosts: &[(u64, String)],
    app_env: &[(String, Vec<EnvVar>)],
    github_ok: bool,
) -> Markup {
    html! {
        dialog id="dlg-add-server" {
            div class="stack" {
                h2 { "Add a server" }
                p class="gate-note" {
                    "DigiHost mints a single-use enrolment code. Run the command it gives you \
                     on the machine you want to manage; the agent bootstraps everything else \
                     from it."
                }
                label class="field" {
                    span { "Region" }
                    input type="text" name="region" placeholder="Helsinki";
                }
                div class="dlg-actions" {
                    button class="btn btn-outline" data-close { "Cancel" }
                    button class="btn btn-primary" data-action="/actions/add-server"
                        data-result="#enroll-result" { "Mint code" }
                }
                pre id="enroll-result" class="result hidden" {}
            }
        }

        dialog id="dlg-app" {
            div class="stack" {
                h2 { "Register an application" }
                p class="gate-note" {
                    "Point DigiHost at a repository and it will look inside to work out how to \
                     deploy it. Everything it proposes can be changed."
                }
                label class="field" {
                    span { "GitHub repository" }
                    div class="row" {
                        input type="text" name="repo" placeholder="owner/name" required;
                        button class="btn btn-outline" data-action="/actions/detect" data-detect { "Inspect" }
                    }
                }
                @if github_ok {
                    div class="field" {
                        span { "Or pick one the App can see" }
                        select data-repo-picker { option value="" { "Loading…" } }
                    }
                }
                div id="detect-result" class="detected hidden" {}
                label class="field" {
                    span { "Name" }
                    input type="text" name="name" placeholder="billing-portal" required;
                }
                label class="field" {
                    span { "Default branch" }
                    input type="text" name="branch" value="main";
                }
                label class="field" {
                    span { "Strategy" }
                    select name="strategy" {
                        option value="Static files" { "Static files" }
                        option value="Dockerfile" { "Dockerfile" }
                        option value="Docker Compose" { "Docker Compose" }
                        option value="systemd unit" { "systemd unit" }
                        option value="IIS site swap" { "IIS site swap" }
                        option value="Windows Service" { "Windows Service" }
                    }
                }
                label class="field" {
                    span { "Entrypoint" }
                    input type="text" name="entrypoint"
                        placeholder="blank if the service or site already exists";
                }
                label class="field" {
                    span { "Port" }
                    input type="text" name="port" placeholder="only for HTTP strategies";
                }
                label class="field" {
                    span { "Deploy path" }
                    input type="text" name="deploy_path"
                        placeholder="blank uses this strategy's conventional location";
                }
                label class="field" {
                    span { "Visibility" }
                    select name="visibility" {
                        option value="public" { "Public repository" }
                        option value="private" disabled[!github_ok] {
                            @if github_ok { "Private — via GitHub App" }
                            @else { "Private — connect a GitHub App first" }
                        }
                    }
                }
                div class="dlg-actions" {
                    button class="btn btn-outline" data-close { "Cancel" }
                    button class="btn btn-primary" data-action="/actions/register-app" { "Register" }
                }
            }
        }

        dialog id="dlg-deploy" {
            div class="stack" {
                h2 { "New deployment" }
                @if apps.is_empty() || hosts.is_empty() {
                    p class="gate-note" {
                        "You need at least one registered application and one enrolled host."
                    }
                    div class="dlg-actions" {
                        button class="btn btn-outline" data-close { "Close" }
                    }
                } @else {
                    label class="field" {
                        span { "Application" }
                        select name="app_id" data-app-picker {
                            @for (id, name, strategy) in apps {
                                option value=(id) data-strategy=(strategy) { (name) }
                            }
                        }
                    }
                    label class="field" {
                        span { "Target host" }
                        select name="host_id" {
                            @for (id, name) in hosts {
                                option value=(id) { (name) }
                            }
                        }
                    }
                    label class="field" {
                        span { "Branch, tag or commit" }
                        input type="text" name="git_ref" placeholder="blank uses the default branch";
                    }
                    label class="field" {
                        span { "Strategy" }
                        select name="strategy" data-strategy-picker {
                            option value="Static files" { "Static files" }
                            option value="Dockerfile" { "Dockerfile" }
                            option value="Docker Compose" { "Docker Compose" }
                            option value="systemd unit" { "systemd unit" }
                            option value="IIS site swap" { "IIS site swap" }
                            option value="Windows Service" { "Windows Service" }
                        }
                    }
                    div class="dlg-actions" {
                        button class="btn btn-outline" data-close { "Cancel" }
                        button class="btn btn-primary" data-action="/actions/deploy" { "Queue deployment" }
                    }
                }
            }
        }

        dialog id="dlg-env" {
            div class="stack" {
                h2 { "Environment" }
                p class="gate-note" {
                    "Configuration is held by this server and handed to an agent only for its \
                     own deployments. It is never stored in the control plane, where every \
                     agent in the fleet could read it."
                }

                @if apps.is_empty() {
                    p class="gate-note" { "Register an application first." }
                    div class="dlg-actions" {
                        button class="btn btn-outline" data-close { "Close" }
                    }
                } @else {
                    label class="field" {
                        span { "Application" }
                        select name="app" {
                            @for (_, name, _) in apps {
                                option value=(name) { (name) }
                            }
                        }
                    }

                    @for (name, vars) in app_env {
                        @if !vars.is_empty() {
                            div class="env-set" {
                                div class="env-head" { (name) }
                                @for var in vars {
                                    div class="env-row" {
                                        span class="env-key" { (var.key) }
                                        span class="env-val" { (var.display_value()) }
                                        @if var.secret {
                                            span class="pill warn" { "secret" }
                                        }
                                        button class="ghost" data-action="/actions/env/unset"
                                            data-field-app=(name) data-field-key=(var.key) { "Remove" }
                                    }
                                }
                            }
                        }
                    }

                    label class="field" {
                        span { "Variables" }
                        textarea name="vars" rows="6"
                            placeholder="DATABASE_URL=postgres://…\nPORT=8080" {}
                    }
                    label class="check" {
                        input type="checkbox" name="secret" value="1";
                        span { "Store as secrets — values are never shown again" }
                    }
                    div class="dlg-actions" {
                        button class="btn btn-outline" data-close { "Close" }
                        button class="btn btn-primary" data-action="/actions/env"
                            data-result="#env-result" { "Save" }
                    }
                    pre id="env-result" class="result hidden" {}
                }
            }
        }

        dialog id="dlg-github" {
            div class="stack" {
                h2 { "GitHub App" }
                p class="gate-note" {
                    "Private repositories are read with installation tokens minted here. The \
                     private key stays on this server and is never sent to an agent. Restart \
                     DigiHost after changing credentials."
                }
                label class="field" {
                    span { "App ID" }
                    input type="text" name="app_id" placeholder="123456" required;
                }
                label class="field" {
                    span { "Private key (PEM)" }
                    textarea name="private_key_pem" rows="6"
                        placeholder="-----BEGIN RSA PRIVATE KEY-----" required {}
                }
                label class="field" {
                    span { "Webhook secret" }
                    input type="text" name="webhook_secret" placeholder="optional";
                }
                div class="dlg-actions" {
                    button class="btn btn-outline" data-close { "Cancel" }
                    @if github_ok {
                        button class="btn btn-outline" data-action="/actions/github/disconnect" { "Disconnect" }
                    }
                    button class="btn btn-primary" data-action="/actions/github" { "Save" }
                }
            }
        }
    }
}

// --------------------------------------------------------------------- pages

/// Sign-in, or first-run setup when nobody has claimed the instance yet.
pub fn login(instance: &str, setup: bool) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head(if setup { "Set up DigiHost" } else { "Sign in · DigiHost" }))
            body {
                div class="gate" {
                    div class="gate-card panel" {
                        div class="brand" style="padding:0;border:0;margin-bottom:24px" {
                            span style="color:var(--accent);display:flex" { (icon::servers(28)) }
                            span class="brand-name" { "DigiHost" }
                        }
                        @if setup {
                            h1 style="font-size:22px;line-height:28px" { "Claim this instance" }
                            p class="gate-note" {
                                "Nobody has set up " (instance) " yet. Choose an operator \
                                 password of at least 12 characters — it is the only \
                                 credential for this instance."
                            }
                        } @else {
                            h1 style="font-size:22px;line-height:28px" { "Sign in" }
                            p class="gate-note" { (instance) }
                        }
                        form method="post" action=(if setup { "/setup" } else { "/login" }) class="stack" {
                            label class="field" {
                                span { "Username" }
                                input type="text" name="username" required
                                    placeholder=(if setup { "admin" } else { "" })
                                    value=(if setup { "admin" } else { "" })
                                    autocomplete="username";
                            }
                            label class="field" {
                                span { "Password" }
                                input type="password" name="password" required
                                    autocomplete=(if setup { "new-password" } else { "current-password" });
                            }
                            button class="btn btn-primary" type="submit" {
                                @if setup { "Claim instance" } @else { "Sign in" }
                            }
                        }
                    }
                }
            }
        }
    }
}


/// A settings-page shell: sidebar plus one content column.
#[allow(clippy::too_many_arguments)]
fn settings_shell(
    title: &str,
    active: &str,
    snap: &FleetSnapshot,
    instance: &str,
    github_ok: bool,
    version: &str,
    update: Option<&str>,
    user: &str,
    content: Markup,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head(title))
            body {
                div class="shell" {
                    (sidebar(snap, instance, github_ok, active, version, update, user))
                    main class="main" { (content) }
                }
                script { (PreEscaped(APP_JS)) }
            }
        }
    }
}

fn ago(created_unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = now.saturating_sub(created_unix) / 86_400;
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{n} days ago"),
    }
}

/// The Team page: the accounts that can operate this instance.
#[allow(clippy::too_many_arguments)]
pub fn team_page(
    snap: &FleetSnapshot,
    instance: &str,
    github_ok: bool,
    version: &str,
    update: Option<&str>,
    user: &str,
    is_admin: bool,
    members: &[(String, bool, u64)],
) -> Markup {
    let content = html! {
        div class="page-head" {
            div {
                h1 { "Team" }
                div class="page-sub" {
                    span {
                        (members.len())
                        (if members.len() == 1 { " member" } else { " members" })
                        " · every member can operate the fleet; administrators also manage this page"
                    }
                }
            }
            @if is_admin {
                div class="actions" {
                    button class="btn btn-primary" data-open="dlg-add-user" {
                        (icon::plus(16)) "Add member"
                    }
                }
            }
        }

        div class="panel table" {
            div class="tr thead" {
                div class="c-host" { "Member" }
                div class="c-platform" { "Role" }
                div class="c-when" { "Added" }
                div class="c-actions" style="width:220px;flex-basis:220px" {}
            }
            @for (name, admin, created) in members {
                div class="tr" {
                    div class="c-host" {
                        div class="cell-title" { (name) }
                        @if name == user { div class="cell-sub" { "you" } }
                    }
                    div class="c-platform" {
                        @if *admin { span class="pill info" { "Administrator" } }
                        @else { span class="pill idle" { "Operator" } }
                    }
                    div class="c-when" { (ago(*created)) }
                    div class="c-actions" style="width:220px;flex-basis:220px;gap:8px" {
                        @if is_admin {
                            button class="ghost" data-reset-user=(name) { "Reset password" }
                            @if name != user {
                                button class="ghost" data-action="/actions/team/remove"
                                    data-field-user=(name)
                                    data-confirm=(format!("Remove {name}? Their sessions end immediately.")) {
                                    "Remove"
                                }
                            }
                        }
                    }
                }
            }
        }

        div class="panel" style="padding:24px;max-width:480px" {
            div class="stack" {
                h2 { "Your password" }
                label class="field" {
                    span { "Current password" }
                    input type="password" name="current" autocomplete="current-password";
                }
                label class="field" {
                    span { "New password" }
                    input type="password" name="password" autocomplete="new-password";
                }
                div class="dlg-actions" style="justify-content:flex-start" {
                    button class="btn btn-primary" data-action="/actions/password" { "Change password" }
                }
            }
        }

        @if is_admin {
            dialog id="dlg-add-user" {
                div class="stack" {
                    h2 { "Add a member" }
                    label class="field" {
                        span { "Username" }
                        input type="text" name="username" placeholder="lowercase, digits, - _ ." required;
                    }
                    label class="field" {
                        span { "Password" }
                        input type="password" name="password" placeholder="at least 12 characters" required;
                    }
                    label class="check" {
                        input type="checkbox" name="admin" value="1";
                        span { "Administrator — can manage members and API tokens" }
                    }
                    div class="dlg-actions" {
                        button class="btn btn-outline" data-close { "Cancel" }
                        button class="btn btn-primary" data-action="/actions/team/add" { "Add" }
                    }
                }
            }

            dialog id="dlg-reset" {
                div class="stack" {
                    h2 { "Reset password" }
                    p class="gate-note" {
                        "Sets a new password for " span data-reset-label class="mono" { "…" } "."
                    }
                    input type="hidden" name="user" value="";
                    label class="field" {
                        span { "New password" }
                        input type="password" name="password" placeholder="at least 12 characters" required;
                    }
                    div class="dlg-actions" {
                        button class="btn btn-outline" data-close { "Cancel" }
                        button class="btn btn-primary" data-action="/actions/team/reset" { "Set password" }
                    }
                }
            }
        }
    };
    settings_shell("Team · DigiHost", "team", snap, instance, github_ok, version, update, user, content)
}

/// The API tokens page: bearer tokens for scripting the operator actions.
#[allow(clippy::too_many_arguments)]
pub fn tokens_page(
    snap: &FleetSnapshot,
    instance: &str,
    github_ok: bool,
    version: &str,
    update: Option<&str>,
    user: &str,
    is_admin: bool,
    tokens: &[(String, u64)],
) -> Markup {
    let content = html! {
        div class="page-head" {
            div {
                h1 { "API tokens" }
                div class="page-sub" {
                    span {
                        "Bearer tokens for scripting the action endpoints — "
                        span class="mono" { "Authorization: Bearer <token>" }
                        ". Tokens deploy and operate; they cannot manage accounts."
                    }
                }
            }
        }

        @if is_admin {
            div class="panel" style="padding:24px;max-width:560px" {
                div class="stack" {
                    h2 { "Mint a token" }
                    label class="field" {
                        span { "Name" }
                        input type="text" name="name" placeholder="ci-deploys" required;
                    }
                    div class="dlg-actions" style="justify-content:flex-start" {
                        button class="btn btn-primary" data-action="/actions/tokens/mint"
                            data-result="#token-result" { "Mint" }
                    }
                    pre id="token-result" class="result hidden" {}
                }
            }
        }

        div class="panel table" {
            div class="tr thead" {
                div class="c-host" { "Token" }
                div class="c-when" { "Created" }
                div class="c-actions" {}
            }
            @for (name, created) in tokens {
                div class="tr" {
                    div class="c-host" { div class="cell-title" { (name) } }
                    div class="c-when" { (ago(*created)) }
                    div class="c-actions" {
                        @if is_admin {
                            button class="ghost" data-action="/actions/tokens/revoke"
                                data-field-name=(name)
                                data-confirm=(format!("Revoke {name}? Anything using it stops working immediately.")) {
                                "Revoke"
                            }
                        }
                    }
                }
            }
            @if tokens.is_empty() {
                div class="empty" {
                    p class="empty-title" { "No tokens" }
                    p class="empty-body" { "Mint one to script deployments from CI or the shell." }
                }
            }
        }
    };
    settings_shell("API tokens · DigiHost", "tokens", snap, instance, github_ok, version, update, user, content)
}

/// The Fleet page.
#[allow(clippy::too_many_arguments)]
pub fn page(
    snap: &FleetSnapshot,
    instance: &str,
    apps: &[(u64, String, String)],
    hosts: &[(u64, String)],
    app_env: &[(String, Vec<EnvVar>)],
    github_ok: bool,
    version: &str,
    update: Option<&str>,
    user: &str,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head("Fleet · DigiHost"))
            body {
                div class="shell" {
                    (sidebar(snap, instance, github_ok, "fleet", version, update, user))

                    main class="main" {
                        div class="page-head" {
                            div {
                                h1 { "Fleet" }
                                div class="page-sub" {
                                    span class="fleet-summary" { (snap.summary()) }
                                    span class="live" {
                                        span class="dot" {}
                                        span class="live-text" { "Live" }
                                    }
                                }
                            }
                            div class="actions" {
                                button class="btn btn-outline" data-open="dlg-add-server" {
                                    (icon::plus(16)) "Add server"
                                }
                                button class="btn btn-primary" data-open="dlg-deploy" {
                                    (icon::deploy(16)) "New deployment"
                                }
                            }
                        }

                        div id="fleet" style="display:flex;flex-direction:column;gap:24px;flex:1;min-height:0" {
                            (fleet_body(snap))
                        }
                    }
                }
                aside id="drawer" class="drawer hidden" {}
                (dialogs(apps, hosts, app_env, github_ok))
                script { (PreEscaped(APP_JS)) }
            }
        }
    }
}

/// The Applications page: every registered application, how it deploys, and
/// where it last got to.
#[allow(clippy::too_many_arguments)]
pub fn applications_page(
    snap: &FleetSnapshot,
    instance: &str,
    overviews: &[AppOverview],
    apps: &[(u64, String, String)],
    hosts: &[(u64, String)],
    app_env: &[(String, Vec<EnvVar>)],
    github_ok: bool,
    version: &str,
    update: Option<&str>,
    user: &str,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head("Applications · DigiHost"))
            body {
                div class="shell" {
                    (sidebar(snap, instance, github_ok, "applications", version, update, user))

                    main class="main" {
                        div class="page-head" {
                            div {
                                h1 { "Applications" }
                                div class="page-sub" {
                                    span {
                                        @if overviews.is_empty() { "Nothing registered yet" }
                                        @else {
                                            (overviews.len())
                                            (if overviews.len() == 1 { " application" } else { " applications" })
                                        }
                                    }
                                }
                            }
                            div class="actions" {
                                button class="btn btn-primary" data-open="dlg-app" {
                                    (icon::plus(16)) "Register application"
                                }
                            }
                        }

                        div class="panel table" {
                            div class="tr thead" {
                                div class="c-host" { "Application" }
                                div class="c-target" { "Source" }
                                div class="c-platform" { "Deploys as" }
                                div class="c-meter" { "Environment" }
                                div class="c-status" { "Last deployment" }
                                div class="c-actions" {}
                            }
                            @for app in overviews {
                                div class="tr" {
                                    div class="c-host" {
                                        div class="cell-title" { (app.name) }
                                        @if !app.deploy_path.is_empty() {
                                            div class="cell-sub mono" { (app.deploy_path) }
                                        }
                                    }
                                    div class="c-target" {
                                        div style="min-width:0" {
                                            div class="cell-title mono"
                                                style="font-size:13px;font-weight:400;color:var(--ink-1)" {
                                                (app.repo)
                                            }
                                            div class="cell-sub" {
                                                (app.branch)
                                                @if app.private { " · private" } @else { " · public" }
                                            }
                                        }
                                    }
                                    div class="c-platform" {
                                        div class="cell-title" style="font-size:13px" { (app.strategy) }
                                        @if !app.entrypoint.is_empty() {
                                            div class="cell-sub mono" { (app.entrypoint) }
                                        }
                                        @if app.port > 0 {
                                            div class="cell-sub" { "port " (app.port) }
                                        }
                                    }
                                    div class="c-meter" {
                                        @if app.env_count == 0 {
                                            span class="cell-sub" { "none" }
                                        } @else {
                                            (app.env_count)
                                            (if app.env_count == 1 { " variable" } else { " variables" })
                                        }
                                    }
                                    div class="c-status" {
                                        span class=(format!("pill {}", tone_class(app.last_tone))) {
                                            (app.last_status)
                                        }
                                    }
                                    div class="c-actions" {
                                        button class="ghost" data-action="/actions/delete-app"
                                            data-field-app_id=(app.id)
                                            data-confirm=(format!("Remove {}? Deployment history stays and nothing running is touched.", app.name)) {
                                            "Remove"
                                        }
                                    }
                                }
                            }
                            @if overviews.is_empty() {
                                div class="empty" {
                                    p class="empty-title" { "No applications yet" }
                                    p class="empty-body" {
                                        "Register one and DigiHost will look inside the \
                                         repository to propose how to deploy it."
                                    }
                                }
                            }
                        }
                    }
                }
                (dialogs(apps, hosts, app_env, github_ok))
                script { (PreEscaped(APP_JS)) }
            }
        }
    }
}
