//! Running a deployment on this host.
//!
//! The agent never holds a GitHub credential or an application's secrets of
//! its own. It asks DigiHost Server for a deployment's source and environment
//! and gets them over its authenticated channel; the server is the only
//! component that talks to GitHub or stores configuration. That split is
//! forced, not stylistic: SpacetimeDB 2.0.2 has no enforced row-level
//! security, so anything one agent could read from the database, every agent
//! could.
//!
//! A deployment is: fetch the tree at a commit, fetch the environment, unpack
//! into a fresh release directory, then run the strategy's commands there,
//! streaming every line of output back into the control plane as it appears.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};

use crate::module_bindings::{append_log, finish_deployment, start_deployment, DbConnection};
use crate::probe::MANAGED_PREFIX;

// The .env writer needs literal quote and backslash characters; spelling them
// as escape constants keeps the format strings legible.
const QUOTE: &str = "\u{22}";
const ESCAPED_QUOTE: &str = "\u{5c}\u{22}";
const BACKSLASH: &str = "\u{5c}";
const DOUBLE_BACKSLASH: &str = "\u{5c}\u{5c}";

/// What counts as success for a step.
///
/// Not every useful program uses "0 means fine": robocopy packs a bitfield
/// into its exit code, and stopping a service that is not running is not an
/// error when the point of the step is "make sure it is stopped".
#[derive(Clone, Copy, PartialEq, Debug)]
enum Success {
    /// Exit code 0.
    Zero,
    /// Exit code below 8 — robocopy's convention, where 1..=7 report what was
    /// copied and 8+ are genuine failures.
    Robocopy,
    /// Run it, log it, carry on regardless. For steps whose failure is the
    /// normal case (stopping something that is not running yet).
    Tolerant,
    /// Exit code 0, or one of these. Use this instead of `Tolerant` when only
    /// specific failures are expected, so the ones that are not — a permission
    /// error, say — still stop the deployment with the real reason instead of
    /// being swallowed and resurfacing later as something misleading.
    Also(&'static [i32]),
}

impl Success {
    fn accepts(self, code: Option<i32>) -> bool {
        match self {
            Success::Tolerant => true,
            Success::Zero => code == Some(0),
            Success::Robocopy => matches!(code, Some(c) if c < 8),
            Success::Also(extra) => matches!(code, Some(c) if c == 0 || extra.contains(&c)),
        }
    }
}

/// One command to run, with a human label for the log.
struct Step {
    label: &'static str,
    program: String,
    args: Vec<String>,
    ok: Success,
}

/// How to bring an application's target into existence when it does not exist
/// yet. An empty `entrypoint` means "do not try" — DigiHost then only deploys
/// into something the operator set up themselves.
pub struct Target {
    pub entrypoint: String,
    pub port: u16,
    /// Where the release is installed. Empty means the strategy's convention.
    pub deploy_path: String,
}

impl Target {
    fn can_create(&self) -> bool {
        !self.entrypoint.trim().is_empty()
    }

    fn install_dir(&self, fallback: &str) -> String {
        let chosen = self.deploy_path.trim();
        if chosen.is_empty() {
            fallback.to_string()
        } else {
            chosen.to_string()
        }
    }
}

pub struct Executor {
    pub server_url: String,
    pub agent_token: String,
    pub root: PathBuf,
    http: reqwest::blocking::Client,
}

impl Executor {
    pub fn new(server_url: String, agent_token: String, root: PathBuf) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            server_url,
            agent_token,
            root,
            http,
        })
    }

    /// Claim a queued deployment and see it through. Never panics: a failure
    /// here must still be reported to the control plane, or the deployment
    /// hangs in Running forever.
    pub fn run(
        &self,
        conn: &DbConnection,
        id: u64,
        app: &str,
        sha: &str,
        strategy: &str,
        target: &Target,
    ) {
        let seq = AtomicU32::new(0);
        let log = |stream: &str, text: String| {
            let n = seq.fetch_add(1, Ordering::SeqCst);
            // Truncate: a stray binary blob in build output should not be
            // pushed row by row into the database.
            let text = if text.len() > 2000 {
                let cut = text
                    .char_indices()
                    .take_while(|(i, _)| *i < 2000)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("{}…", &text[..cut])
            } else {
                text
            };
            if let Err(e) = conn.reducers.append_log(id, n, stream.to_string(), text) {
                tracing::warn!("could not append deploy log: {e}");
            }
        };

        if let Err(e) = conn.reducers.start_deployment(id) {
            tracing::warn!("could not claim deployment {id}: {e}");
            return;
        }
        tracing::info!(deployment = id, app, sha, strategy, "deploying");

        match self.execute(id, app, sha, strategy, target, &log) {
            Ok(()) => {
                log("stdout", "Deployment succeeded.".to_string());
                report(conn, id, true);
            }
            Err(e) => {
                log("stderr", format!("Deployment failed: {e:#}"));
                report(conn, id, false);
            }
        }
    }

    fn execute(
        &self,
        id: u64,
        app: &str,
        sha: &str,
        strategy: &str,
        target: &Target,
        log: &dyn Fn(&str, String),
    ) -> Result<()> {
        let release = self.root.join("apps").join(app).join("releases").join(sha);

        // A retry of the same commit must not inherit half-written files from
        // the attempt that failed.
        if release.exists() {
            std::fs::remove_dir_all(&release)
                .with_context(|| format!("clearing {}", release.display()))?;
        }
        std::fs::create_dir_all(&release)
            .with_context(|| format!("creating {}", release.display()))?;

        log("stdout", format!("Fetching {app} at {sha}"));
        let tarball = self.fetch_source(id, sha, log)?;

        // Values are deliberately never logged — only the names.
        let env = self.fetch_env(id)?;
        if !env.is_empty() {
            log(
                "stdout",
                format!(
                    "Applying {} environment variable(s): {}",
                    env.len(),
                    env.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            );
        }

        log("stdout", format!("Unpacking into {}", release.display()));
        unpack(&tarball, &release)?;
        let _ = std::fs::remove_file(&tarball);

        // Compose reads `.env` from the project directory, docker run takes it
        // via --env-file, and it gives the deployed application somewhere to
        // read its own configuration from.
        write_env_file(&release, &env)?;

        for step in steps(strategy, app, &release, target, sha, !env.is_empty())? {
            log("stdout", format!("$ {} {}", step.program, step.args.join(" ")));
            self.run_step(&step, &release, &env, log)?;
        }

        mark_current(&self.root, app, sha)?;
        Ok(())
    }

    /// Ask the server for this deployment's source. The bearer token is the
    /// agent's own; the server checks the deployment belongs to this host.
    fn fetch_source(&self, id: u64, sha: &str, log: &dyn Fn(&str, String)) -> Result<PathBuf> {
        let url = format!(
            "{}/api/deployments/{id}/source",
            self.server_url.trim_end_matches('/')
        );

        let mut resp = self
            .http
            .get(&url)
            .bearer_auth(&self.agent_token)
            .send()
            .with_context(|| format!("requesting source from {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("server refused the source request ({status}): {body}");
        }

        let path = std::env::temp_dir().join(format!("digihost-{sha}.tar.gz"));
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let bytes = std::io::copy(&mut resp, &mut file).context("downloading source")?;
        log("stdout", format!("Fetched {bytes} bytes"));
        Ok(path)
    }

    /// This deployment's environment, from the server. The agent holds no
    /// configuration of its own: the server decides what this host may see,
    /// which keeps one host's secrets away from every other host.
    fn fetch_env(&self, id: u64) -> Result<BTreeMap<String, String>> {
        let url = format!(
            "{}/api/deployments/{id}/env",
            self.server_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.agent_token)
            .send()
            .with_context(|| format!("requesting environment from {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("server refused the environment request ({status}): {body}");
        }
        resp.json().context("parsing the environment")
    }

    fn run_step(
        &self,
        step: &Step,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        log: &dyn Fn(&str, String),
    ) -> Result<()> {
        let mut child = Command::new(&step.program)
            .args(&step.args)
            .current_dir(cwd)
            .envs(env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {} ({})", step.program, step.label))?;

        // Drain stderr on its own thread; otherwise a chatty build fills the
        // pipe buffer and the child blocks forever while we read stdout.
        let stderr = child.stderr.take();
        let stderr_lines = std::thread::spawn(move || {
            let mut collected = Vec::new();
            if let Some(err) = stderr {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    collected.push(line);
                }
            }
            collected
        });

        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                log("stdout", line);
            }
        }

        let status = child.wait().context("waiting for step to finish")?;
        for line in stderr_lines.join().unwrap_or_default() {
            log("stderr", line);
        }

        if !step.ok.accepts(status.code()) {
            bail!(
                "{} failed with exit code {}",
                step.label,
                status.code().unwrap_or(-1)
            );
        }
        if !status.success() && step.ok == Success::Tolerant {
            log(
                "stdout",
                format!(
                    "({} exited {} — continuing)",
                    step.label,
                    status.code().unwrap_or(-1)
                ),
            );
        }
        Ok(())
    }
}

fn report(conn: &DbConnection, id: u64, ok: bool) {
    if let Err(e) = conn.reducers.finish_deployment(id, ok) {
        tracing::error!("could not report deployment {id} outcome: {e}");
    }
}

/// Commands for a strategy.
///
/// Paths and names follow a convention rather than per-application ceremony:
/// a DigiHost-managed unit, service, site or container is named
/// `digihost-<app>`, which is also the prefix the workload probe filters on.
fn steps(
    strategy: &str,
    app: &str,
    release: &Path,
    target: &Target,
    sha: &str,
    has_env: bool,
) -> Result<Vec<Step>> {
    let unit = format!("{MANAGED_PREFIX}{app}");
    let dir = release.display().to_string();

    let step = |label: &'static str, program: &str, args: &[&str], ok: Success| Step {
        label,
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        ok,
    };

    Ok(match strategy {
        // Needs nothing installed on the host and no elevation: put the files
        // where they belong and stop. Whatever fronts them serves them.
        "Static files" => {
            let install = target.install_dir(&default_static_dir(app));
            vec![if cfg!(target_os = "windows") {
                step(
                    "publish files",
                    "robocopy",
                    &[&dir, &install, "/MIR", "/NFL", "/NDL", "/NJH", "/NJS"],
                    Success::Robocopy,
                )
            } else {
                step(
                    "publish files",
                    "sh",
                    &["-c", &format!("mkdir -p '{install}' && cp -a ./. '{install}/'")],
                    Success::Zero,
                )
            }]
        }

        // A bare Dockerfile: build an image tagged with the commit, then
        // replace the running container.
        "Dockerfile" => {
            let short = &sha[..sha.len().min(7)];
            let tag = format!("{unit}:{short}");
            let mut plan = vec![
                step("docker build", "docker", &["build", "-t", &tag, "."], Success::Zero),
                // Removing a container that does not exist is the first-deploy case.
                step("remove old container", "docker", &["rm", "-f", &unit], Success::Tolerant),
            ];
            let mut run: Vec<String> = ["run", "-d", "--name"]
                .iter()
                .map(|a| a.to_string())
                .collect();
            run.push(unit.clone());
            run.extend(["--restart".to_string(), "unless-stopped".to_string()]);
            if has_env {
                // Given at run rather than baked at build, so a configuration
                // change never needs an image rebuild.
                run.extend(["--env-file".to_string(), ".env".to_string()]);
            }
            if target.port > 0 {
                run.push("-p".to_string());
                run.push(format!("{0}:{0}", target.port));
            }
            run.push(tag);
            plan.push(Step {
                label: "docker run",
                program: "docker".to_string(),
                args: run,
                ok: Success::Zero,
            });
            plan
        }

        "Docker Compose" => {
            // The project name must be pinned: compose derives it from the
            // directory otherwise, and every release deploys from a new
            // SHA-named directory — each deploy would create a fresh project
            // and leave the previous one's containers running forever.
            let project = unit.to_lowercase();
            vec![
                step(
                    "compose build",
                    "docker",
                    &["compose", "-p", &project, "build"],
                    Success::Zero,
                ),
                step(
                    "compose up",
                    "docker",
                    &["compose", "-p", &project, "up", "-d", "--remove-orphans"],
                    Success::Zero,
                ),
            ]
        }

        "systemd unit" => {
            let install = target.install_dir(&format!("/opt/digihost/{app}"));
            let mut plan = vec![step(
                "install release",
                "sh",
                &["-c", &format!("mkdir -p '{install}' && cp -a ./. '{install}/'")],
                Success::Zero,
            )];
            if target.can_create() {
                // Written every deployment so the unit tracks the application's
                // configuration instead of drifting. The dash on
                // EnvironmentFile makes it optional, so an application with no
                // configuration still starts.
                let unit_file = format!(
                    "[Unit]\nDescription=DigiHost {app}\nAfter=network.target\n\n\
                     [Service]\nWorkingDirectory={install}\nEnvironmentFile=-{install}/.env\n\
                     ExecStart={}\nRestart=always\n\n\
                     [Install]\nWantedBy=multi-user.target\n",
                    target.entrypoint.trim()
                );
                plan.push(step(
                    "write unit",
                    "sh",
                    &[
                        "-c",
                        &format!(
                            "cat > /etc/systemd/system/{unit}.service <<'DIGIHOST_UNIT'\n{unit_file}DIGIHOST_UNIT"
                        ),
                    ],
                    Success::Zero,
                ));
            }
            plan.push(step("reload units", "systemctl", &["daemon-reload"], Success::Zero));
            if target.can_create() {
                plan.push(step("enable service", "systemctl", &["enable", &unit], Success::Zero));
            }
            plan.push(step("restart service", "systemctl", &["restart", &unit], Success::Zero));
            plan
        }

        "IIS site swap" => {
            let site_path = target.install_dir(&format!("C:\\inetpub\\digihost\\{app}"));
            let mut plan = vec![
                step(
                    "stop site",
                    "appcmd",
                    &["stop", "site", &format!("/site.name:{unit}")],
                    Success::Tolerant,
                ),
                step(
                    "copy release",
                    "robocopy",
                    &[&dir, &site_path, "/MIR", "/NFL", "/NDL", "/NJH", "/NJS"],
                    Success::Robocopy,
                ),
            ];
            if target.port > 0 {
                // Tolerant: appcmd refuses to add a site that already exists,
                // which is the normal case after the first deployment.
                plan.push(step(
                    "create site",
                    "appcmd",
                    &[
                        "add",
                        "site",
                        &format!("/name:{unit}"),
                        &format!("/physicalPath:{site_path}"),
                        &format!("/bindings:http/*:{}:", target.port),
                    ],
                    Success::Tolerant,
                ));
            }
            plan.push(step(
                "start site",
                "appcmd",
                &["start", "site", &format!("/site.name:{unit}")],
                Success::Zero,
            ));
            plan
        }

        "Windows Service" => {
            let install = target.install_dir(&format!("C:\\ProgramData\\DigiHost\\{app}"));
            let mut plan = vec![
                step("stop service", "sc", &["stop", &unit], Success::Tolerant),
                step(
                    "copy release",
                    "robocopy",
                    &[&dir, &install, "/MIR", "/NFL", "/NDL", "/NJH", "/NJS"],
                    Success::Robocopy,
                ),
            ];
            if target.can_create() {
                plan.push(step(
                    "create service",
                    "sc",
                    &[
                        "create",
                        &unit,
                        &format!("binPath= {install}\\{}", target.entrypoint.trim()),
                        "start=",
                        "auto",
                    ],
                    // 1073 is "service already exists" — the normal case on
                    // every deployment after the first. Anything else (access
                    // denied, say) fails here with the real reason instead of
                    // two steps later with something misleading.
                    Success::Also(&[1073]),
                ));
            }
            plan.push(step("start service", "sc", &["start", &unit], Success::Zero));
            plan
        }

        other => bail!("unknown deployment strategy: {other}"),
    })
}

/// Where static content goes when the application does not say.
fn default_static_dir(app: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("C:\\inetpub\\digihost\\{app}")
    } else {
        format!("/srv/digihost/{app}")
    }
}

/// Write the deployment's configuration next to its code.
///
/// Values are quoted so spaces survive, and the file is rewritten every
/// deployment so a variable removed in the interface actually disappears from
/// the host.
fn write_env_file(release: &Path, env: &BTreeMap<String, String>) -> Result<()> {
    if env.is_empty() {
        return Ok(());
    }

    let mut body = String::new();
    for (key, value) in env {
        let escaped = value
            .replace(BACKSLASH, DOUBLE_BACKSLASH)
            .replace(QUOTE, ESCAPED_QUOTE);
        body.push_str(&format!("{key}={QUOTE}{escaped}{QUOTE}\n"));
    }

    let path = release.join(".env");
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;

    // Best effort: on Unix keep it to the owner. On Windows it inherits the
    // deployment directory, which belongs to the agent.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Record which release is live. A plain file rather than a symlink: Windows
/// needs a privilege for symlinks that an agent should not require.
fn mark_current(root: &Path, app: &str, sha: &str) -> Result<()> {
    let dir = root.join("apps").join(app);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("CURRENT"), sha).context("recording current release")
}

/// Unpack a GitHub tarball, dropping its single top-level wrapper directory
/// (`owner-repo-<sha>/`), which nobody wants in their deployment path.
fn unpack(tarball: &Path, into: &Path) -> Result<()> {
    let file =
        std::fs::File::open(tarball).with_context(|| format!("opening {}", tarball.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    let mut entries = 0usize;
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading a tar entry")?;
        let path = entry.path().context("reading entry path")?.into_owned();

        let mut parts = path.components();
        parts.next(); // drop the wrapper directory
        let stripped: PathBuf = parts.collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }

        // Refuse anything that climbs out of the release directory. Tar files
        // are attacker-influenced input the moment a repository is.
        if stripped
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("refusing tar entry with a parent path: {}", path.display());
        }

        let dest = into.join(&stripped);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("unpacking {}", stripped.display()))?;
        entries += 1;
    }

    if entries == 0 {
        bail!("source archive was empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "abcdef1234567";

    fn no_target() -> Target {
        Target {
            entrypoint: String::new(),
            port: 0,
            deploy_path: String::new(),
        }
    }

    fn with_target() -> Target {
        Target {
            entrypoint: "billing.exe".to_string(),
            port: 8080,
            deploy_path: String::new(),
        }
    }

    fn at_path(path: &str) -> Target {
        Target {
            entrypoint: String::new(),
            port: 0,
            deploy_path: path.to_string(),
        }
    }

    #[test]
    fn known_strategies_produce_steps() {
        for s in [
            "Static files",
            "Dockerfile",
            "Docker Compose",
            "systemd unit",
            "IIS site swap",
            "Windows Service",
        ] {
            let plan = steps(s, "billing", Path::new("/tmp/rel"), &no_target(), SHA, false).unwrap();
            assert!(!plan.is_empty(), "{s} produced no steps");
        }
    }

    #[test]
    fn unknown_strategy_is_refused() {
        assert!(
            steps("interpretive dance", "billing", Path::new("/tmp/rel"), &no_target(), SHA, false)
                .is_err()
        );
    }

    #[test]
    fn managed_names_carry_the_probe_prefix() {
        let plan =
            steps("systemd unit", "billing", Path::new("/tmp/rel"), &with_target(), SHA, false)
                .unwrap();
        let restart = plan.last().unwrap();
        assert!(
            restart.args.iter().any(|a| a == "digihost-billing"),
            "service name must carry the prefix the workload probe filters on"
        );
    }

    #[test]
    fn stopping_is_tolerant_so_a_first_deploy_can_succeed() {
        // `sc stop` on a service that does not exist yet returns 1060; a first
        // deployment must not die on that.
        for strategy in ["Windows Service", "IIS site swap"] {
            let plan =
                steps(strategy, "billing", Path::new("/tmp/rel"), &no_target(), SHA, false).unwrap();
            assert_eq!(
                plan[0].ok,
                Success::Tolerant,
                "{strategy}: the stop step must tolerate a missing target"
            );
        }
    }

    #[test]
    fn exit_codes_are_read_per_program() {
        // robocopy uses 1..=7 to report what it copied, not to report failure.
        assert!(Success::Robocopy.accepts(Some(1)));
        assert!(Success::Robocopy.accepts(Some(7)));
        assert!(!Success::Robocopy.accepts(Some(8)));
        assert!(!Success::Zero.accepts(Some(1)));
        assert!(Success::Tolerant.accepts(Some(1060)));
        // "already exists" passes; "access denied" must not.
        assert!(Success::Also(&[1073]).accepts(Some(1073)));
        assert!(!Success::Also(&[1073]).accepts(Some(5)));
    }

    #[test]
    fn creating_a_service_needs_an_entrypoint() {
        let without =
            steps("Windows Service", "billing", Path::new("/tmp/rel"), &no_target(), SHA, false)
                .unwrap();
        assert!(
            !without.iter().any(|s| s.label == "create service"),
            "without an entrypoint DigiHost must not invent a service"
        );

        let with =
            steps("Windows Service", "billing", Path::new("/tmp/rel"), &with_target(), SHA, false)
                .unwrap();
        let create = with.iter().position(|s| s.label == "create service").expect("create step");
        let start = with.iter().position(|s| s.label == "start service").expect("start step");
        assert!(create < start, "the service must exist before it is started");
    }

    #[test]
    fn iis_site_creation_needs_a_port() {
        let without =
            steps("IIS site swap", "billing", Path::new("/tmp/rel"), &no_target(), SHA, false)
                .unwrap();
        assert!(!without.iter().any(|s| s.label == "create site"));

        let with =
            steps("IIS site swap", "billing", Path::new("/tmp/rel"), &with_target(), SHA, false)
                .unwrap();
        let create = with.iter().find(|s| s.label == "create site").expect("create step");
        assert!(
            create.args.iter().any(|a| a.contains("8080")),
            "the binding must carry the port"
        );
    }

    #[test]
    fn systemd_units_are_written_enabled_and_load_the_env_file() {
        let plan =
            steps("systemd unit", "billing", Path::new("/tmp/rel"), &with_target(), SHA, false)
                .unwrap();
        let labels: Vec<&str> = plan.iter().map(|s| s.label).collect();
        assert!(labels.contains(&"write unit"));
        assert!(labels.contains(&"enable service"));

        let write = plan.iter().find(|s| s.label == "write unit").unwrap();
        assert!(write.args.iter().any(|a| a.contains("ExecStart=billing.exe")));
        // The dash makes the env file optional, so an application with no
        // configuration still starts.
        assert!(write.args.iter().any(|a| a.contains("EnvironmentFile=-")));
    }

    #[test]
    fn compose_project_name_is_pinned_and_stable() {
        // Without -p, compose names the project after the release directory,
        // which changes every deploy — old containers would never be replaced.
        let a = steps("Docker Compose", "shop", Path::new("/x/releases/aaa111"), &no_target(), "aaa111", false).unwrap();
        let b = steps("Docker Compose", "shop", Path::new("/x/releases/bbb222"), &no_target(), "bbb222", false).unwrap();
        for plan in [&a, &b] {
            for s in plan.iter() {
                let i = s.args.iter().position(|x| x == "-p").expect("compose must pin -p");
                assert_eq!(s.args[i + 1], "digihost-shop");
            }
        }
    }

    #[test]
    fn dockerfile_builds_then_replaces_the_container() {
        let plan =
            steps("Dockerfile", "api", Path::new("/tmp/rel"), &no_target(), SHA, true).unwrap();
        let labels: Vec<&str> = plan.iter().map(|s| s.label).collect();
        assert_eq!(labels, ["docker build", "remove old container", "docker run"]);
        // The image is tagged with the commit, so releases stay distinguishable.
        assert!(plan[0].args.iter().any(|a| a == "digihost-api:abcdef1"));
        // Removing a container that does not exist yet must not fail a first deploy.
        assert_eq!(plan[1].ok, Success::Tolerant);
    }

    #[test]
    fn dockerfile_env_and_port_are_optional() {
        let plain =
            steps("Dockerfile", "api", Path::new("/tmp/rel"), &no_target(), SHA, false).unwrap();
        let run = plain.last().unwrap();
        assert!(
            !run.args.iter().any(|a| a == "--env-file"),
            "no env, no flag: {:?}",
            run.args
        );
        assert!(!run.args.iter().any(|a| a == "-p"));

        let with =
            steps("Dockerfile", "api", Path::new("/tmp/rel"), &with_target(), SHA, true).unwrap();
        let run = with.last().unwrap();
        assert!(run.args.iter().any(|a| a == "--env-file"));
        assert!(run.args.iter().any(|a| a == "8080:8080"));
    }

    #[test]
    fn static_files_needs_no_service_and_no_elevation() {
        let plan =
            steps("Static files", "site", Path::new("/tmp/rel"), &no_target(), SHA, false).unwrap();
        assert_eq!(plan.len(), 1, "publishing files is a single step");
        assert!(!plan.iter().any(|s| s.program == "sc" || s.program == "systemctl"));
    }

    #[test]
    fn an_explicit_deploy_path_wins_over_the_convention() {
        let custom =
            steps("Static files", "site", Path::new("/tmp/rel"), &at_path("/var/www/site"), SHA, false)
                .unwrap();
        assert!(
            custom[0].args.iter().any(|a| a.contains("/var/www/site")),
            "explicit path must be used: {:?}",
            custom[0].args
        );

        let default =
            steps("Static files", "site", Path::new("/tmp/rel"), &no_target(), SHA, false).unwrap();
        assert!(
            default[0].args.iter().any(|a| a.contains("digihost")),
            "without a path, fall back to the convention: {:?}",
            default[0].args
        );
    }

    #[test]
    fn deploy_path_applies_to_every_strategy_with_an_install_dir() {
        for strategy in ["systemd unit", "IIS site swap", "Windows Service", "Static files"] {
            let plan =
                steps(strategy, "billing", Path::new("/tmp/rel"), &at_path("/custom/here"), SHA, false)
                    .unwrap();
            assert!(
                plan.iter().any(|s| s.args.iter().any(|a| a.contains("/custom/here"))),
                "{strategy} ignored the configured deploy path"
            );
        }
    }

    #[test]
    fn env_file_quotes_and_escapes() {
        let dir = std::env::temp_dir().join("digihost-env-file-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let q = QUOTE;
        let mut env = BTreeMap::new();
        env.insert("PLAIN".to_string(), "value".to_string());
        env.insert("SPACED".to_string(), "two words".to_string());
        env.insert("QUOTED".to_string(), format!("say {q}hi{q}"));
        env.insert("WINPATH".to_string(), format!("C:{BACKSLASH}app"));
        write_env_file(&dir, &env).unwrap();

        let body = std::fs::read_to_string(dir.join(".env")).unwrap();
        assert!(body.contains(&format!("PLAIN={q}value{q}")));
        assert!(
            body.contains(&format!("SPACED={q}two words{q}")),
            "spaces must survive: {body}"
        );
        // Inner quotes are escaped, so a value cannot terminate its own line.
        assert!(
            body.contains(&format!("QUOTED={q}say {ESCAPED_QUOTE}hi{ESCAPED_QUOTE}{q}")),
            "inner quotes must be escaped: {body}"
        );
        assert!(
            body.contains(&format!("WINPATH={q}C:{DOUBLE_BACKSLASH}app{q}")),
            "backslashes must be escaped: {body}"
        );
        assert_eq!(body.lines().count(), 4, "one line per variable: {body}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_env_writes_no_file() {
        let dir = std::env::temp_dir().join("digihost-env-empty-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_env_file(&dir, &BTreeMap::new()).unwrap();
        assert!(
            !dir.join(".env").exists(),
            "an application with no configuration should leave no file behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
