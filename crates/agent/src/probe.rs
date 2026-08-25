//! Host inspection: what this machine is, and what it is currently running.
//!
//! Everything platform-specific lives here. The rest of the agent deals only
//! in [`HostFacts`] and [`WorkloadReport`], so adding a platform means adding
//! a module below and a branch in [`facts`] / [`workloads`] — nothing else
//! moves.

use std::process::Command;

use sysinfo::System;

use crate::module_bindings::{Platform, WorkloadKind, WorkloadReport};

/// Prefix marking a service, unit or container as something DigiHost deployed,
/// as opposed to OS or vendor plumbing that happens to run on the same box.
/// The deploy executor names its targets with the same prefix.
pub const MANAGED_PREFIX: &str = "digihost-";

/// Static-ish identity of this machine, gathered once at enrolment.
pub struct HostFacts {
    pub name: String,
    pub address: String,
    pub platform: Platform,
    pub os_name: String,
    pub runtime: String,
}

/// Instantaneous resource load, sampled every heartbeat.
pub struct Load {
    pub cpu_pct: u8,
    pub mem_pct: u8,
}

/// Run a command and return trimmed stdout, or None if it could not run or
/// exited non-zero. Absence is normal here: a Linux box need not have Docker,
/// a Windows box need not have IIS.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

pub fn load(sys: &mut System) -> Load {
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let cpu = sys.global_cpu_usage().round().clamp(0.0, 100.0) as u8;
    let total = sys.total_memory();
    let mem = if total == 0 {
        0
    } else {
        ((sys.used_memory() as f64 / total as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8
    };

    Load { cpu_pct: cpu, mem_pct: mem }
}

pub fn facts(override_name: Option<String>) -> HostFacts {
    let name = override_name
        .or_else(System::host_name)
        .unwrap_or_else(|| "unnamed-host".to_string());

    HostFacts {
        name,
        address: primary_address(),
        platform: current_platform(),
        os_name: os_name(),
        runtime: runtime(),
    }
}

/// Workloads worth showing an operator.
///
/// Containers and IIS sites are deployment units by nature, so they always
/// count. Services and systemd units are not — a stock server runs hundreds —
/// so only DigiHost-deployed ones are reported unless `include_system` is set.
/// Without this filter the workload count just measures how much vendor
/// software is installed.
pub fn workloads(include_system: bool) -> Vec<WorkloadReport> {
    #[cfg(target_os = "windows")]
    {
        windows::workloads(include_system)
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix::workloads(include_system)
    }
}

fn current_platform() -> Platform {
    if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

fn os_name() -> String {
    System::long_os_version()
        .or_else(System::name)
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// Best-effort primary IPv4. Not authoritative — but far more useful in a
/// host list than a hostname alone.
fn primary_address() -> String {
    #[cfg(target_os = "windows")]
    let found = output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "(Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -ne '127.0.0.1' } | Select-Object -First 1).IPAddress",
        ],
    );
    #[cfg(not(target_os = "windows"))]
    let found = output("sh", &["-c", "hostname -I 2>/dev/null | awk '{print $1}'"]);

    found.unwrap_or_else(|| "unknown".to_string())
}

fn runtime() -> String {
    #[cfg(target_os = "windows")]
    {
        windows::runtime()
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix::runtime()
    }
}

// ------------------------------------------------------------------ Linux/Unix

#[cfg(not(target_os = "windows"))]
mod unix {
    use super::*;

    pub fn runtime() -> String {
        let mut parts = Vec::new();
        if let Some(v) = output("docker", &["--version"]) {
            parts.push(short_version("Docker", &v));
        }
        if let Some(v) = output("podman", &["--version"]) {
            parts.push(short_version("Podman", &v));
        }
        if output("systemctl", &["--version"]).is_some() {
            parts.push("systemd".to_string());
        }
        if parts.is_empty() {
            "no container runtime".to_string()
        } else {
            parts.join(" · ")
        }
    }

    /// "Docker version 27.3.1, build ce12230" -> "Docker 27.3.1"
    fn short_version(label: &str, raw: &str) -> String {
        let version = raw
            .split_whitespace()
            .find(|tok| tok.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("")
            .trim_end_matches(',');
        if version.is_empty() {
            label.to_string()
        } else {
            format!("{label} {version}")
        }
    }

    pub fn workloads(include_system: bool) -> Vec<WorkloadReport> {
        let mut found = Vec::new();
        found.extend(containers("docker", WorkloadKind::DockerContainer));
        found.extend(containers("podman", WorkloadKind::PodmanContainer));
        found.extend(systemd_units(include_system));
        found
    }

    fn containers(engine: &str, kind: WorkloadKind) -> Vec<WorkloadReport> {
        let Some(raw) = output(engine, &["ps", "--all", "--format", "{{.Names}}\t{{.State}}"])
        else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| {
                let (name, state) = line.split_once('\t')?;
                Some(WorkloadReport {
                    name: name.trim().to_string(),
                    kind: kind.clone(),
                    state: state.trim().to_string(),
                })
            })
            .collect()
    }

    fn systemd_units(include_system: bool) -> Vec<WorkloadReport> {
        let Some(raw) = output(
            "systemctl",
            &[
                "list-units",
                "--type=service",
                "--state=running",
                "--no-legend",
                "--no-pager",
                "--plain",
            ],
        ) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| {
                let mut cols = line.split_whitespace();
                let unit = cols.next()?;
                // UNIT LOAD ACTIVE SUB DESCRIPTION…
                let sub = cols.nth(2).unwrap_or("running");
                let name = unit.trim_end_matches(".service");
                if !include_system && !name.starts_with(MANAGED_PREFIX) {
                    return None;
                }
                Some(WorkloadReport {
                    name: name.to_string(),
                    kind: WorkloadKind::SystemdUnit,
                    state: sub.to_string(),
                })
            })
            .collect()
    }
}

// -------------------------------------------------------------------- Windows

#[cfg(target_os = "windows")]
mod windows {
    use super::*;

    const APPCMD: &str = r"C:\Windows\System32\inetsrv\appcmd.exe";

    pub fn runtime() -> String {
        let mut parts = Vec::new();
        if let Some(v) = output("docker", &["--version"]) {
            let version = v
                .split_whitespace()
                .find(|tok| tok.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .unwrap_or("")
                .trim_end_matches(',');
            parts.push(if version.is_empty() {
                "Docker".to_string()
            } else {
                format!("Docker {version}")
            });
        }
        if std::path::Path::new(APPCMD).exists() {
            parts.push("IIS".to_string());
        }
        if let Some(v) = output("dotnet", &["--version"]) {
            parts.push(format!(".NET {}", v.lines().next().unwrap_or(&v).trim()));
        }
        parts.push("Windows Services".to_string());
        parts.join(" · ")
    }

    pub fn workloads(include_system: bool) -> Vec<WorkloadReport> {
        let mut found = containers();
        found.extend(iis_sites());
        found.extend(services(include_system));
        found
    }

    fn containers() -> Vec<WorkloadReport> {
        let Some(raw) = output("docker", &["ps", "--all", "--format", "{{.Names}}\t{{.State}}"])
        else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| {
                let (name, state) = line.split_once('\t')?;
                Some(WorkloadReport {
                    name: name.trim().to_string(),
                    kind: WorkloadKind::DockerContainer,
                    state: state.trim().to_string(),
                })
            })
            .collect()
    }

    /// `appcmd list sites` prints e.g.
    /// SITE "Default Web Site" (id:1,bindings:http/*:80:,state:Started)
    fn iis_sites() -> Vec<WorkloadReport> {
        if !std::path::Path::new(APPCMD).exists() {
            return Vec::new();
        }
        let Some(raw) = output(APPCMD, &["list", "sites"]) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| {
                let name = line.split('"').nth(1)?;
                let state = line
                    .split("state:")
                    .nth(1)
                    .and_then(|rest| rest.split(')').next())
                    .unwrap_or("Unknown")
                    .trim()
                    .to_string();
                Some(WorkloadReport {
                    name: name.to_string(),
                    kind: WorkloadKind::IisSite,
                    state,
                })
            })
            .collect()
    }

    /// Running, automatic-start services — and only DigiHost's unless asked,
    /// because the full service table on a Windows box is hundreds of rows of
    /// OS plumbing.
    fn services(include_system: bool) -> Vec<WorkloadReport> {
        let Some(raw) = output(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "Get-Service | Where-Object { $_.Status -eq 'Running' -and $_.StartType -eq 'Automatic' } | ForEach-Object { \"$($_.Name)`t$($_.Status)\" }",
            ],
        ) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| {
                let (name, state) = line.split_once('\t')?;
                let name = name.trim();
                if !include_system && !name.starts_with(MANAGED_PREFIX) {
                    return None;
                }
                Some(WorkloadReport {
                    name: name.to_string(),
                    kind: WorkloadKind::WindowsService,
                    state: state.trim().to_string(),
                })
            })
            .collect()
    }
}
