# Software Design Document

| Field | Value |
|---|---|
| Product | Gantry |
| Version covered | 0.1.x, with the Docker Engine API engine layer |
| Status | Draft for review |
| Related | [Requirements](requirements.md), [Test plan](test-plan.md), [File format](../compose-spec.md) |

Requirement IDs in brackets, such as [FR-POD-01], point to the
[requirements](requirements.md).

## 1. Overview

Gantry is one Rust library with two binaries:

* `gantry`, the command line tool. It also runs the per-interface DHCP
  agent as the hidden subcommand `gantry internal dhcp-agent`.
* `gantry-oci`, a small OCI runtime shim that Docker calls instead of
  `runc` so Docker containers can join namespaces by path.

The central idea is that the pod is a host object, not a container. Gantry
creates and pins the namespaces, wires the network, writes the shared files
and loads the firewall before any engine is involved. Engines are then asked
to start containers inside namespaces that already exist.

```
                          operator (root)
                                |
                           gantry CLI
                                |
          +---------------------+----------------------+
          |                     |                      |
   compose loader          pod manager             engine layer
   (YAML -> Project)   (netns, links, DHCP,     (Docker Engine API
                        files, nftables)          over Unix sockets)
                                |                      |
                         /run/netns, /run/gantry     +--> dockerd --> gantry-oci --> runc
                         LAN via macvlan/ipvlan        +--> kiri daemon
```

## 2. Module structure

| Module | Path | Responsibility |
|---|---|---|
| CLI | `src/bin/gantry.rs` | Argument parsing (clap), command dispatch, log streaming, `setup` commands, the hidden DHCP agent entry point. |
| Compose loader | `src/compose/` | Reads and merges files (`mod.rs`), deserializes raw YAML (`raw.rs`), interpolates variables (`interpolate.rs`), parses durations and sizes (`units.rs`), and validates and normalizes into the project model (`normalize.rs`). |
| Project model | `src/project.rs` | Normalized, validated types: `Project`, `Pod` (with its network `Attachment`s), `Service`, `Volume`, firewall rules, `RuntimeKind`. Saves and loads the project snapshot. |
| Pod manager | `src/pod/mod.rs` | `ensure` (create or reuse), `remove`, `refresh` (hosts files and firewalls), config hashing, DHCP agent start and stop. |
| Namespaces | `src/pod/ns.rs` | Creating, pinning, entering and removing network, UTS and IPC namespaces. Running a command inside a netns. |
| Links | `src/pod/link.rs` | rtnetlink: macvlan and ipvlan creation, renaming, addresses, routes, MAC derivation, gratuitous ARP. |
| DHCP | `src/pod/dhcp.rs` | DHCPv4 client and agent loop, lease file, PID and log files. |
| Shared files | `src/pod/files.rs` | Rendering and writing `hosts`, `resolv.conf`, `hostname`. |
| Pod state | `src/pod/state.rs` | Paths under the run directory and the `PodState` record. |
| Firewall | `src/firewall.rs` | Renders a pod's rules as an nftables ruleset text. |
| Engine | `src/engine/` | `mod.rs`: one client per runtime over the Docker Engine API (bollard), socket selection, container, image, volume and log operations. `spec.rs`: the create body for a service in its pod. `exec.rs`: exec with TTY and resize. `auth.rs`: registry credentials. `build.rs`: image builds through the engine CLI. |
| OCI shim | `src/oci.rs`, `src/bin/gantry-oci.rs` | Parses the runtime invocation, rewrites the bundle's `config.json` from annotations, execs the real runtime. |
| Health watcher | `src/heal.rs` | `watch`: acts on unhealthy containers that set `health_on_failure`; restart backoff, host lock, action reports (section 6.2). |
| Orchestrator | `src/orchestrator.rs` | `up`, `down`, `start`, `stop`, dependency ordering, waiting on conditions, the per-project lock. |
| Errors | `src/errors.rs` | One `thiserror` enum grouped by layer: Config, Io, Sys, Engine, Network, Firewall, Dhcp, State. |
| Utilities | `src/util.rs` | Atomic and in-place writes, hashing. |

## 3. Compose loading

1. Locate files (or take `-f` values), read them, merge left to right
   [FR-CMP-01].
2. Interpolate variables from the environment and `.env` [FR-CMP-02].
3. Deserialize into the raw model and warn on ignored keys [FR-CMP-04].
   `build:` sections are kept for the build step [FR-CMP-09].
4. Normalize: resolve the project name [FR-CMP-08], apply profiles, place
   services without `pod:` in the only pod and resolve `network_mode`
   [FR-POD-06], resolve runtimes (project, pod, service, then `--runtime`)
   [FR-RT-08], validate each pod network entry and default its gateway to
   the first host of its subnet [FR-NET-05, FR-ADDR-02], validate hostnames, DNS options
   and firewall fields [NFR-SEC-03], reject reserved labels [FR-CMP-06] and
   pod settings on pod members [FR-CMP-05].

The result is an immutable `Project`. Everything after this step works from
it. `gantry config` prints it [FR-CLI-06].

## 4. Pods

### 4.1 Creation

`pod::ensure` compares a hash of the pod's shape with the saved `PodState`.
If the hash matches and the netns is still pinned, the pod is reused.
Otherwise it is removed and created again [FR-POD-07]:

1. Pin a network namespace at `/run/netns/gt-<project>-<pod>` [FR-POD-01].
   A stale one from a crash is removed first [NFR-REL-02].
2. If shared, pin UTS (setting hostname and domain name inside it) and IPC
   namespaces in the pod directory [FR-POD-02, FR-POD-03].
3. Bring up `lo`, write pod sysctls including the unprivileged port and ping
   defaults, and disable IPv6 if asked [FR-POD-08, FR-POD-09].
4. For each network entry, create a macvlan (`bridge`) or ipvlan (`l2`)
   link on its parent from the host namespace directly into the pod
   namespace, rename it and set the MAC. The link inherits the parent's MTU
   [FR-NET-01 to FR-NET-04, FR-NET-08].
5. Static networks: add addresses and the default route, send gratuitous ARP
   [FR-ADDR-02, FR-NET-05, FR-NET-06]. DHCP networks: set `rp_filter` to 0 on
   the interface and spawn a DHCP agent (section 4.3).
6. Write `resolv.conf`, an initial `hosts` and `hostname` into the pod
   directory [FR-DNS-01, FR-DNS-04].
7. Save `state.json` with a fresh random pod id.

If any step fails, `pod::remove` runs before the error is returned
[FR-POD-10].

### 4.2 Refresh

`pod::refresh` runs after all pods in an `up` exist, from `firewall reload`,
and from a DHCP agent whose address changed. For every pod it rewrites the
hosts file in place (loopback names, other pods' first IPv4 and IPv6
addresses, `extra_hosts`) [FR-DNS-02, FR-DNS-03, FR-DNS-05] and reapplies
the firewall [FR-FW-01]. It continues past a failing pod and returns the
first error [NFR-REL-01].

### 4.3 DHCP agent

The agent is `gantry internal dhcp-agent`, started detached with its
output in `dhcp-<if>.log`. It enters the pod netns, opens a UDP socket bound
to the device with the broadcast flag, and runs DISCOVER, OFFER, REQUEST,
ACK with a client identifier `gantry-<project>-<pod>-<if>` [FR-ADDR-03]. On
bind it applies the address and route, sends gratuitous ARP, writes
`dhcp-<if>.json` and, when the pod has no `dns:`, rewrites `resolv.conf`. It
renews at T1, rebinds at T2, drops the address at expiry, and releases on
SIGTERM or SIGINT [FR-ADDR-04]. If the bound address differs from the
previous one, it loads the project snapshot and runs `pod::refresh`
[FR-ADDR-05]. Lease fields are validated before use [FR-ADDR-06].

`pod::remove` stops agents by PID file, after checking the PID's
`/proc/<pid>/cmdline` [NFR-SEC-04], with SIGTERM then SIGKILL after five
seconds.

### 4.4 Removal

`pod::remove` stops DHCP agents (which release leases), unmounts and deletes
the netns, UTS and IPC pins, and deletes the pod directory. Deleting the
netns destroys its links. Containers must already be gone, or the namespaces
stay alive until they exit.

## 5. Firewall

`firewall::render` turns a pod's `firewall:` section (or an empty one when
the pod has none), plus service `ports:` and `expose:` [FR-FW-06], into one
ruleset. Input rules are ordered: the default ICMP and ICMPv6 accepts, the pod's own
rules, `ports:` and `expose:`, then the default reject from `subnet`
[FR-FW-03]:

```
table inet gantry {          # replaced atomically with nft -f
  chain input  { type filter hook input  priority filter; policy <input>;  ... }
  chain output { type filter hook output priority filter; policy <output>; ... }
}
```

The fixed preamble allows loopback, established and related, and DHCP
replies on DHCP pods. When a chain's policy is closed and the defaults do not
already accept all ICMP, it also allows ICMP errors and IPv6 neighbor
discovery [FR-FW-04]. Every rule carries a counter, and each chain ends with
a counter-only rule for its policy; `firewall show` parses `nft list table`
into a hit-count table [FR-FW-09]. Peers are resolved through an address book built
from saved pod state and leases. An unknown peer is skipped for accept rules
and is an error for deny rules [FR-FW-07]. `subnet` resolves to the subnets
of the pod's static addresses and current leases. On a DHCP pod without a
lease it is empty and the rule is skipped until the agent reloads the
firewall with the lease. `reject` is rendered as an
explicit reject statement since base chain policies only take accept or drop.

The ruleset is loaded by running `nft -f -` inside the pod namespace
(`pod::nft`), so the host's tables are never touched [FR-FW-02]. The `nft`
CLI is kept on purpose for readable output (DECISIONS item 20).

### 5.1 Moving data

`datamove::plan` compares each service's running container (its mounts as
the engine reports them, so a named volume's real mountpoint on any engine)
with the project file. Every target whose source changed becomes a move,
grouped by old and new path [FR-CLI-17]. `datamove::run` holds the project
lock throughout: live rsync passes, `stop` of the services that use the
data, a final pass, a `--checksum --dry-run` verification, then
`up_locked` for those services. A `.gantry-move` marker sits in the
destination until verification passes. `up` calls `refuse_unmoved` under
the same lock, so it never recreates a service on an empty or half-copied
path [FR-CLI-18]. rsync is a process rather than a library on purpose: it
handles hard links, ACLs, extended attributes, sparse files and devices,
and the package depends on it.

## 6. Engine layer

### 6.1 One API client for both runtimes

Docker is the primary runtime and kiri the second. Both speak the Docker Engine HTTP API, so `Engine` wraps one
`bollard::Docker` client per runtime, connected over a Unix socket
[FR-RT-01]:

| Runtime | Socket | Override |
|---|---|---|
| Docker | `/var/run/docker.sock` [FR-RT-02] | `GANTRY_DOCKER_HOST` |
| kiri | `/run/kiri/docker.sock` [FR-RT-03] | `GANTRY_KIRI_HOST` |

Clients are created lazily, only for runtimes the command needs. `Engine::check`
pings the socket and reports the path that failed [FR-RT-12]. Calls use the
bollard request timeout [NFR-REL-04], except log follow and attached exec.

This replaces the earlier design, which ran the engine CLIs
as subprocesses (DECISIONS item 21). The API removes the CLI dependency,
avoids parsing CLI output, returns typed errors, and reuses one connection
per engine [NFR-PERF-05]. The one remaining subprocess is the image build
(section 6.8).

### 6.2 Self-healing watcher

`src/heal.rs` holds the health watcher [FR-HEAL-03 to FR-HEAL-09]. Docker
and kiri run healthchecks and set `State.Health.Status`, but neither acts
on `unhealthy`; restart policies cover exits only. The watcher fills that
gap for services that opt in.

* **Opt in.** `health_on_failure` is stored as the container label
  `io.gantry.health-on-failure` [FR-HEAL-02]. The watcher needs only the
  engines: it lists running containers with that label
  (`GET /containers/json?filters={"label":["io.gantry.health-on-failure"],"status":["running"]}`)
  and inspects each. Projects appear and disappear without a reload.
* **Poll, not events.** Every 10 seconds per engine. Healthchecks run every
  30 seconds or so, the list is small, and polling behaves the same on
  Docker and kiri.
* **Guard.** For an unhealthy container it takes the project's command lock
  (`/run/gantry/locks/<project>.lock`) without waiting and inspects again under it [FR-HEAL-04].
  `gantry up`, `down`, `data move` and `gantry boot start` hold that lock,
  so boot priority waits and deploys are never interrupted. A stopped
  container is never listed, so `gantry stop` and `down` are not undone.
* **Restart.** `POST /containers/{id}/restart`, which keeps the container
  ID and its stop timeout. History is kept per container ID: the first
  restart is immediate, later ones wait 30 s doubling to 5 min, and after 5
  restarts within 30 minutes the watcher reports `gave_up` and leaves the
  container until it is healthy again or recreated [FR-HEAL-05].
* **Stop.** `POST /containers/{id}/stop`. Docker treats that as a manual
  stop, so `unless-stopped` does not bring it back [FR-HEAL-06].
* **One per host.** `watch` holds `flock` on `/run/gantry/heal.lock`
  [FR-HEAL-07].
* **Library first.** `heal::watch(runtimes, policy, control, actions,
  cancel)` takes a `tokio_util` `CancellationToken`, a `Control` for
  per-project pause and resume, and an optional `mpsc::Sender<HealAction>`
  fed with `try_send` [FR-HEAL-08]. Cancellation is checked between
  actions, so the future resolves after any restart in progress.
  `gantry supervise` and `gantry-supervise.service` run it for Docker and
  kiri [FR-HEAL-10]; gantry-agent runs it in-process and disables the unit.

### 6.3 Operations used

| Operation | API |
|---|---|
| Reachability | `GET /_ping`, `GET /version` |
| Image present, pull | `GET /images/{name}/json`, `POST /images/create` (streamed progress, `X-Registry-Auth` from the auth files) |
| Image default command | `GET /images/{name}/json` (`Config.Cmd`) |
| Create, start, stop, remove | `POST /containers/create`, `/start`, `/stop?t=`, `DELETE /containers/{id}?v=true&force=true` |
| List, inspect | `GET /containers/json?all=true&filters={"label":["io.gantry.project=<p>"]}`, `GET /containers/{id}/json` |
| Logs | `GET /containers/{id}/logs` with `follow`, `tail`, stdout and stderr |
| Exec | `POST /containers/{id}/exec`, `POST /exec/{id}/start` (attached, with TTY when requested), `POST /exec/{id}/resize` on terminal resize, `GET /exec/{id}/json` for the exit code |
| Health waits | `GET /containers/{id}/json` (`State.Health.Status`). |
| Self-healing | `GET /containers/json` filtered on the heal label and `running`, `POST /containers/{id}/restart`, `POST /containers/{id}/stop` (section 6.2). |
| Volumes | `POST /volumes/create`, `GET /volumes/{name}`, `DELETE /volumes/{name}` |

All of these go through the API; no CLI is involved [FR-RT-14].

Registry credentials for pulls come from the same places the CLIs read
[FR-RT-13], in the order the runtime's own CLI reads them, including
`credHelpers` and `credsStore` helpers:

* Docker and kiri: `$DOCKER_CONFIG/config.json`, default
  `~/.docker/config.json` of root.
Credentials are passed per request and never written to disk by Gantry.

### 6.4 Concurrency

Engine work runs in parallel where order allows [NFR-PERF-06]:

* images are pulled (and built) concurrently before any pod is touched;
* services are grouped into dependency levels from `depends_on`; services
  in one level are created and started concurrently, and a level starts
  once the previous level's conditions are met;
* `down` stops and removes one level at a time, in reverse order, with the
  containers of each level handled concurrently.

Pod creation and the firewall refresh stay sequential, since they share the
project lock and the address book.

### 6.5 Translating a service

`create_args` becomes a builder that fills a create request: name, image, command, entrypoint,
environment, user, working directory, labels, mounts, tmpfs, devices,
capabilities, security options, resources, ulimits, healthcheck, restart
policy, stop signal and init.

Every container gets these labels:

| Label | Value |
|---|---|
| `io.gantry.project` | Project name |
| `io.gantry.service` | Service name |
| `io.gantry.pod` | Pod name |
| `io.gantry.pod-id` | Random id of the pod instance |
| `io.gantry.config-hash` | Hash of the create request |

A container is reused when its config hash matches, and recreated otherwise
[FR-CLI-02]. Since the pod id is part of the hash, containers from a
previous pod instance (after a reboot or a pod recreate) are always replaced
[FR-ST-02].

The pod's `hosts`, `resolv.conf` and `hostname` are added as read-only bind
mounts on all runtimes [FR-DNS-01].

### 6.6 Joining the pod

| Runtime | HostConfig |
|---|---|
| Docker | `Runtime: "gantry"`, `NetworkMode: "none"`, `Annotations: {io.gantry.netns, io.gantry.utsns, io.gantry.ipcns}`. Hostname only when UTS is not shared. [FR-RT-05] |
| kiri | `NetworkMode: "ns:<netns>"`, `UTSMode: "ns:<uts>"`, `IpcMode: "ns:<ipc>"`, hostname when UTS is not shared. Needs both patches in `contrib/kiri`. [FR-RT-06] |

### 6.7 The Docker shim

Docker cannot join a namespace by path. `gantry setup docker` adds this to
`/etc/docker/daemon.json` and reloads `dockerd`:

```json
{ "runtimes": { "gantry": { "path": "/usr/local/bin/gantry-oci" } } }
```

Docker copies container annotations into the OCI bundle. On `create`,
`gantry-oci` reads `config.json` from the bundle, and for each annotated
namespace:

1. checks the path is under an allowed directory and has no `..`
   [NFR-SEC-02];
2. checks the namespace exists, failing with a hint to rerun `gantry up`;
3. sets the matching `linux.namespaces[].path`.

It removes `hostname` and `domainname` when UTS is shared, removes `net.*`
sysctls when the netns is shared, writes the spec back, and execs `runc` (or
`GANTRY_OCI_RUNTIME`, read from dockerd's environment, never from an
annotation) with the original arguments. Other subcommands pass straight
through. Docker restarts call the shim again, so restarted containers rejoin
the pod [FR-ST-04].

### 6.8 Image builds

Builds are the one place Gantry runs an engine CLI [FR-CMP-09]. A build is
slow compared with process startup, and the CLIs give BuildKit,
`.dockerignore` handling and build secrets with Compose semantics for free.

| Runtime | Build command |
|---|---|
| Docker | `docker build` against the Docker socket (`DOCKER_HOST` set from `GANTRY_DOCKER_HOST`) |
| kiri | `docker build` against the kiri socket with `DOCKER_BUILDKIT=0`, since kiri implements the classic build endpoint only |

Compose build keys (context, dockerfile, args, target, labels, secrets and
the rest) map to the CLI flags. The image is tagged with the service's
image name. Extra `build.tags` are added. `gantry build [svc...]` builds on demand
and `up --build` builds before starting [FR-CLI-11]; plain `up` builds only
images that are missing. The CLI is needed only on hosts with projects that
use `build:`.

## 7. Data flows

### 7.1 `gantry up`

```
CLI          load + normalize project, take --runtime
Orchestrator start order from depends_on (topological, selected services + deps)
Engine       ping every engine the selected services use       [FR-RT-12]
Orchestrator take /run/gantry/<project>/lock (flock)          [FR-CLI-09]
Engine       build images with build: (if missing or --build), pull others
             (pull policy), all concurrently; ensure named volumes
for each pod used:
  Orchestrator if pod hash changed or netns gone: remove its containers
  Pod          ensure: reuse, or create (netns, uts, ipc, links, addresses
               or DHCP agent, files, state.json)
Project      save snapshot to /run/gantry/<project>/project.json
Pod          refresh: hosts files and firewalls for every pod
for each dependency level, services in the level concurrently:
  Orchestrator wait for depends_on conditions                     [FR-CLI-08]
  Engine       remove same-named container on another runtime     [FR-RT-11]
  Engine       inspect; reuse if hash matches (start if stopped),
               else stop, remove, create, start
if --wait: wait for running or healthy
if not -d: follow logs; on interrupt stop services, keep pods      [FR-CLI-03]
```

Images are pulled and built before anything else so a failed pull or build
leaves nothing half started.

### 7.2 `gantry down`

```
Orchestrator take the project lock
Engine       list containers labeled io.gantry.project=<project> on every engine
Engine       stop (grace period or -t) and remove, one dependency level at a
             time in reverse order, each level concurrently
for each pod with state or a pinned netns:
  Pod        stop DHCP agents (release leases), remove netns, uts, ipc, pod dir
if -v: remove non-external named volumes on every engine
Orchestrator remove lock, snapshot and project directory
```

### 7.3 DHCP address change

```
agent: lease bound with a new address
  -> apply address, route, gratuitous ARP, write dhcp-<if>.json, resolv.conf
  -> load /run/gantry/<project>/project.json
  -> pod::refresh: rewrite every hosts file, reapply every firewall
```

## 8. State layout

All state is in tmpfs and container labels [FR-ST-01]. Nothing survives a
reboot, by design.

```
/run/netns/
  gt-<project>-<pod>              pinned network namespace (ip netns visible)
/run/gantry/
  heal.lock                       flock held by the one health watcher on the host
  locks/<project>.lock            flock for commands that change the project
  <project>/
    project.json                  normalized project snapshot (refresh source)
    pods/<pod>/
      state.json                  PodState: id, config hash, namespace paths, interfaces
      uts                         pinned UTS namespace (when shared)
      ipc                         pinned IPC namespace (when shared)
      hosts                       bind mounted at /etc/hosts
      resolv.conf                 bind mounted at /etc/resolv.conf
      hostname                    bind mounted at /etc/hostname
      dhcp-<if>.pid               DHCP agent PID
      dhcp-<if>.json              current lease
      dhcp-<if>.log               DHCP agent log
```

`GANTRY_RUN_DIR` and `GANTRY_NETNS_DIR` move these trees for testing.

## 9. Security model

### 9.1 Trust boundaries

| Boundary | Trust |
|---|---|
| Operator to Gantry | The operator is root. The project file is trusted to describe intent but is still validated, because its values reach the kernel, nftables and files that containers read. |
| Gantry to engines | Engine sockets are root-equivalent. Gantry connects only to fixed root-owned socket paths. |
| dockerd to `gantry-oci` | The shim runs as root under dockerd. Annotations on a container are treated as untrusted input, since any Docker API user can set them. |
| Containers to host | Containers share only the pod namespaces. The PID namespace is never shared. `/etc` files are read only. |
| LAN to pod | Governed by the pod firewall. Without one, the pod is open (an accepted assumption). |
| DHCP server to pod | Lease contents are untrusted and validated before use. |

### 9.2 Controls

* Shim namespace paths are restricted to `/run/netns/`, `/var/run/netns/` and
  `/run/gantry/`, with no `..`, and must exist. An annotation cannot pick
  the runtime binary [NFR-SEC-02]. A Docker API user can therefore only join
  namespaces in those directories, which they could already reach as a
  Docker user with host bind mounts.
* `io.gantry.*` labels are reserved so a project file cannot impersonate
  another project's containers [FR-CMP-06].
* Firewall comments, limits and interface names, hostnames, DNS search and
  options, and DHCP domain names are validated [FR-FW-08, NFR-SEC-03,
  FR-ADDR-06].
* Deny rules fail closed when a peer has no address [FR-FW-07].
* DHCP agent PIDs are verified before signaling [NFR-SEC-04].
* Namespace pins and state files are created race-free and written
  atomically [NFR-SEC-05].
* No `unwrap`, `expect` or `panic` outside tests [NFR-SEC-07]. Ids come from
  `getrandom` [NFR-SEC-08].
* Gantry opens no network listener. The reverse proxy and TLS offload
  assumption for web services does not apply.

### 9.3 Known risks

* `GANTRY_*` environment overrides (run directory, engine sockets) are read
  by a root process. sudo's default `env_reset` strips them, but a sudoers
  rule that keeps the environment would let a caller redirect them.
  `GANTRY_NFT` is already limited to debug builds [NFR-SEC-10]; the others
  remain an open item.
* A pod without `firewall:` is reachable from the whole LAN.

## 10. Failure handling

| Failure | Behavior |
|---|---|
| Engine socket unreachable | `up` stops before taking the lock or changing anything, naming the socket. |
| Image pull or build fails | `up` stops before any pod or container is created. |
| Pod creation fails | Partial pod removed, error returned. |
| Container create or start fails | Error returned; pods and earlier services remain so the operator can inspect and rerun `up`. |
| Firewall apply fails for one pod | Other pods are still refreshed; the first error is returned. |
| DHCP server silent | The agent keeps retrying; services start without an address until a lease binds. |
| Netns missing when Docker starts a container | The shim fails with a hint to rerun `gantry up`. |
| Crash or reboot leaves stale state | Next `up` replaces stale namespaces and containers whose pod id no longer matches. |
| Build tool missing | Error naming the missing `docker` CLI, only for services with `build:`. |
| Service stays unhealthy | With `health_on_failure`, restarted with backoff or stopped (section 6.2); without it, left as the engine marks it. |
| Engine unreachable while watching | The watcher logs it once, skips that engine and tries again each poll. |

## 11. Deployment

* Binaries in `/usr/bin` (package) or `/usr/local/bin` (source build):
  `gantry`, `gantry-oci`.
* Docker hosts: shim registered in `/etc/docker/daemon.json`.
* Every host: `gantry-supervise.service` enabled, unless gantry-agent runs
  the watcher.
* kiri hosts: patched kiri serving `/run/kiri/docker.sock`.
* Projects for boot: `<parent>/<name>/gantry.yaml`, where the parents are
  listed in `/etc/gantry/project-dirs` (`/etc/gantry`, `/mnt/*`),
  registered in `/var/lib/gantry/boot/<name>` by `gantry up -d` and started
  by `gantry-boot.service` in pod priority order (`boot::start`).

Procedures are in the [runbooks](../runbooks/README.md).

## 12. Open items

* A plain kiri build works against kiri's classic build endpoint; build
  keys beyond context, Dockerfile and args are not yet verified on kiri
  (section 6.8).
* The kiri patches are local and not yet proposed upstream.
* Socket and directory `GANTRY_*` overrides remain in release builds;
  the `nft` binary override is debug only.
