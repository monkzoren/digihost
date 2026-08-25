# DigiHost plan

Self-hosted deployment manager for mixed Linux and Windows fleets. One
instance per organisation, all Rust, SpacetimeDB as both datastore and control
plane. Rebuilt from scratch on 2026-08-24; everything below reflects the
current codebase.

## Done — and verified against a live instance

- **Control plane** (`crates/module`) — nine tables, every state change a
  reducer. Operator actions guarded by an instance claim; agent actions bound
  to the caller's own host; scheduled reaper marks silent agents offline.
- **Host agent** (`crates/agent`) — bootstraps from one enrolment code
  (exchanged over HTTP for its token and the control plane address), identity
  in its state directory so restarts reconnect as the same host, heartbeats
  with a workload census, and executes deployments.
- **Six strategies** — Static files, Dockerfile, Docker Compose, systemd unit,
  IIS site swap, Windows Service. Per-application entrypoint, port and deploy
  path; target creation on first deploy when an entrypoint is given;
  per-program exit-code semantics (robocopy bitfield, `sc` 1073/1060).
- **GitHub source brokering** (`crates/server`) — public repos anonymously,
  private through a GitHub App (JWT → cached installation tokens). PEM and App
  ID validated at save; connect and disconnect from the interface.
- **Guided registration** — Inspect reads the repository root and proposes a
  strategy with its reasoning, honestly marked when unsure; repo picker when
  an App is connected.
- **Configuration and secrets** — held by the server, never in SpacetimeDB
  (RLS is unenforced there); delivered per-deployment over the agent's
  authenticated channel; secrets write-only in the UI, names-only in logs.
- **Webhooks** — HMAC-verified push deliveries redeploy to the hosts an
  application already runs on. Verified against GitHub's documented example
  signature and live end-to-end (signed push → queued → deployed green).
- **Rollback that redeploys** — marks the record and queues the previous
  successful release; refuses when there is nothing earlier.
- **Web interface** — Fleet (live over SSE: stats, host table with drain and
  resume, deployments with live log drawer) and Applications; login/setup
  gate; every unbuilt nav destination visibly marked *soon*.
- **`--manage-spacetime`** — the server starts and owns the database process.

Test suite: 40 (15 agent, 25 server). End-to-end proof on this machine:
enrol → detect → register → deploy commit A → deploy commit B → roll back to
A → webhook push redeploys B — all green, secret never observable anywhere.

## The honest gap to a Coolify-class product

- **Domains, TLS, reverse proxy** — a deployed site has no URL managed for it.
  The biggest remaining subsystem.
- **Databases / one-click services** — nothing provisions a Postgres.
- **Persistent volumes** — unmodelled.
- **The Linux paths have never executed.** Docker/Podman/systemd probes and
  strategies are written and unit-tested but this machine is Windows; they
  need a real Linux host before being called working.
- **Dockerfile strategy is untested live** — no Docker on this machine; the
  step plans are unit-tested only.
- **Thirteen nav destinations** (Servers detail, Agents, Logs, Metrics,
  Alerts, Team, API tokens, …) are marked *soon*.
- **No TLS on the interface itself** — fine behind a reverse proxy, and the
  session cookie should gain `Secure` when that lands.
- **Agents connect to SpacetimeDB directly**, so that port must be reachable
  from every managed host; proxying it through the server would leave agents
  needing one address.
- No notifications, exec-into-workload, PR previews, or public API.

## Principles

- Single-tenant. No org, tenant or billing concepts anywhere.
- The control plane is the database; no API tier creeps back in for fleet
  state.
- Browsers never talk to SpacetimeDB; only agents do.
- Secrets never enter SpacetimeDB while RLS is unenforced.
- Counts on screen mean something: a workload is a thing DigiHost deployed.
- Verdicts, not optimism: every operator action waits for the control plane's
  actual answer, and every guess the product makes about a repository is
  inspectable and editable.
