# DigiHost

A deployment manager for mixed **Linux and Windows** fleets.

DigiHost is self-hosted: you run one instance, it manages your servers. There
is no DigiHost cloud and no multi-tenancy — one instance is one organisation's
fleet.

## Shape

```
Your server
└── digihost-server          the thing you install
    ├── SpacetimeDB          all fleet data, and the control plane itself
    ├── web interface        the only UI
    └── GitHub App           mints installation tokens, fetches source

Your managed hosts (Linux + Windows)
└── digihost-agent           enrols once, then reports and deploys
```

The control plane **is** the database: tables hold the fleet, reducers are the
only way state changes. Agents talk to SpacetimeDB directly; browsers never
do — the server renders every page and pushes changes over SSE.

Everything is Rust. Agents are a single binary with no runtime to install on a
managed host, and they never hold a GitHub credential or another host's
secrets.

## Why secrets and source go through the server

SpacetimeDB 2.0.2 ships `client_visibility_filter` but documents it as
**unimplemented and not enforced**. There is no row-level security: a table is
either private — readable by no client — or public, readable by *every*
connected client, and every agent is a client.

So secrets never enter the control plane. The server holds the GitHub App
private key and all application environment; it mints short-lived installation
tokens, and streams source and configuration to the one agent whose deployment
it is, over that agent's own authenticated channel.

## Layout

| Path | What it is |
| --- | --- |
| `crates/module` | The control plane. Compiles to wasm, runs inside SpacetimeDB. |
| `crates/server` | DigiHost Server: web interface, GitHub broker, agent API. |
| `crates/agent` | The host agent: enrols, heartbeats, runs deployments. |

`crates/module` is deliberately **not** a workspace member — it targets
`wasm32-unknown-unknown` and must not share feature resolution with the native
binaries.

## Running it

Publish the control plane:

```bash
spacetime publish --no-config -p ./crates/module --server local -y digihost
```

Start the server. Add `--manage-spacetime` and it starts and owns SpacetimeDB
itself instead of expecting one already running:

```bash
cargo run -p digihost-server -- --instance "Your Ops"
```

Open <http://127.0.0.1:8420>. The first visit asks you to claim the instance
with an operator password (minimum 12 characters); everything sits behind that
from then on. Sessions are in memory, so a server restart signs everyone out.

Then, in the interface, **Add server** mints a single-use enrolment code and
shows the command to run on the machine you want to manage:

```bash
digihost-agent --server http://your-digihost:8420 --enrollment-code <code>
```

The agent exchanges the code for its own bearer token, learns where the
control plane lives, and enrols itself. It only needs the code once; its
identity lives in its state directory, so a restarted agent reconnects as the
same host without any flags.

## Registering an application

Paste a repository as `owner/name` and press **Inspect**. DigiHost reads the
files at the repository root and proposes a strategy, explaining why:

| Found at the root | Proposed |
| --- | --- |
| a compose file | Docker Compose |
| `Dockerfile` | Dockerfile (build + run) |
| `*.csproj` / `*.sln` | IIS site swap, port 80 |
| `package.json` | systemd unit, port 3000 |
| `index.html` | Static files |
| none of the above | Static files, marked *not sure* |

Everything it proposes is editable, and a guess it is not confident about says
so rather than presenting itself as fact. With a GitHub App connected you can
also pick from the repositories the App can see, which fills in the
repository, its default branch and its visibility.

## Deployment strategies

A deployment resolves the reference to a concrete commit, fetches the tree,
fetches the application's environment, unpacks into a fresh release directory,
and runs the strategy's commands there — streaming every line of output back
into the control plane as it happens. Clicking a deployment in the interface
opens its live log.

| Strategy | What it runs |
| --- | --- |
| Static files | publishes the release to the deploy path; no service, no elevation |
| Dockerfile | `docker build -t digihost-<app>:<sha7>`, replace the container, `docker run` with `--restart unless-stopped` |
| Docker Compose | `docker compose build`, then `up -d --remove-orphans` |
| systemd unit | installs the release, writes/enables the unit, `systemctl restart` |
| IIS site swap | stops site `digihost-<app>`, robocopy, starts it |
| Windows Service | stops `digihost-<app>`, robocopy, starts it |

Anything DigiHost manages is named `digihost-<app>` — the same prefix the
workload probe filters on, so the fleet page counts deployed things rather
than every service installed on a box.

**Where files go.** Each application has a **deploy path**; blank uses the
strategy's convention (`/opt/digihost/<app>`, `C:\inetpub\digihost\<app>`,
`C:\ProgramData\DigiHost\<app>`, `/srv/digihost/<app>`).

**Creating the target.** Give an application an **entrypoint** and DigiHost
creates the service or unit on first deployment (`sc create`, or a generated
systemd unit plus `enable`); an IIS site takes a **port** for its binding.
Leave them blank and DigiHost only deploys into a target you set up yourself.
Creating targets needs privilege — Administrator on Windows, root on Linux —
and an unelevated agent fails at the create step saying exactly that.

**Exit codes are read per program.** Robocopy's 1–7 mean "copied things";
`sc create` returning 1073 means the service already exists; stopping
something that is not running is not an error. Anything unexpected fails the
deployment with the real reason.

## Configuration and secrets

Set per-application environment from **Environment** in the sidebar as
`KEY=value` lines (a pasted `.env` file works). Tick *store as secrets* and
the values are never shown again — only names, with the value masked.

On deployment the agent writes a `.env` beside the release (0600 on Linux) and
passes the same variables to every strategy step, so Compose picks them up
automatically, `docker run` takes them via `--env-file`, and generated systemd
units load them via `EnvironmentFile`. The deploy log records variable *names*
only — never values.

## GitHub

**Public repositories** need no setup. **Private repositories** go through a
GitHub App you create in your own org:

1. Settings → Developer settings → GitHub Apps → New GitHub App.
2. Repository permissions → **Contents: Read-only**.
3. Generate a private key, note the App ID, install the App on the account
   that owns your repositories.
4. In DigiHost: **Connect GitHub App**, paste the App ID and the PEM.

The key is validated at save time and written to the server's data directory;
it never leaves. Installation tokens are minted on demand and cached until
shortly before expiry. Restart the server after changing credentials — the
live client caches tokens against what it started with. **Disconnect** in the
same dialog forgets the credentials.

### Webhooks

Point a webhook at `POST /api/github/webhook`, content type
`application/json`, with the same secret you entered, subscribed to **push**
events. A push to an application's default branch redeploys it to every online
host it has already deployed to successfully, reusing each host's last
strategy. Deliveries without a valid signature are rejected; an instance with
no webhook secret refuses the endpoint entirely.

## Rollback

**Roll back** on a succeeded deployment does two things: marks it rolled back,
and queues a deployment of the previous successful release of that application
on that host — so the old code is actually running again, not just recorded as
preferred. If no earlier successful release exists, it refuses and says so.

## Behaviour worth knowing

- **Heartbeats replace a host's whole workload set**, so vanished containers
  and services actually disappear.
- **Draining is sticky.** A heartbeat cannot flip a draining host back online,
  and clearing the flag cannot resurrect a host whose agent is gone.
- **Silent agents are reaped** after 90s, so a crash without a clean
  disconnect still shows as unreachable.
- **An agent can only touch its own host** — enforced in the control plane's
  reducers and again on the server's source/env endpoints.
- **Operator actions are guarded in the control plane too.** The server claims
  the instance on first start; reducers refuse every other identity, so an
  agent cannot mint enrolment codes or queue deployments.
- **The interface waits for the control plane's verdict.** A reducer call only
  confirms the request was *sent*; DigiHost waits for the actual result, so
  refusals surface with their real reason instead of a false "done".
- **Fleet and Applications are the only pages.** The rest of the navigation is
  a roadmap, marked *soon*, rather than links that silently do nothing.

## Updating

Releases are built by CI from tags and published on GitHub. An installed
server shows its version at the bottom of the sidebar, and an amber note
appears there when a newer release exists.

To update a Linux install:

```bash
sudo digihost-update
```

That downloads the latest release, verifies its checksum, swaps the binaries,
and restarts the services. The server republishes the control-plane module on
startup whenever the shipped wasm changed, so additive schema changes ride
along automatically; a breaking schema change stops startup with instructions
instead of limping on mismatched.

Windows agents update by replacing `digihost-agent.exe` from the
`digihost-windows-x86_64.zip` release asset and restarting the process.

## Cutting a release

```bash
deploy/release.sh --patch --deploy
```

The release tool refuses a dirty tree or an out-of-sync main, runs the test
suite before touching anything, bumps the workspace and module versions, tags,
pushes, waits for CI to publish the Linux artifact, and — with `--deploy` —
runs `digihost-update` on `$DIGIHOST_DEPLOY_HOST` (set it in a gitignored
`.release.env`). `--minor`, `--major` or an explicit version work too.

## Version pinning

The `spacetimedb` crate is pinned to the exact CLI version (`=2.0.2`). Cargo
resolves newer 2.x by default and the API moves between minors — `ctx.sender`
became a method, index syntax became `index(accessor = …)`. If you upgrade the
CLI, update the pin and regenerate both binding directories:

```bash
spacetime generate --lang rust --out-dir ./crates/server/src/module_bindings -p ./crates/module
spacetime generate --lang rust --out-dir ./crates/agent/src/module_bindings -p ./crates/module
```
