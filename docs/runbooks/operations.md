# Operating and troubleshooting

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.1.x with the Docker Engine API engine layer |
| Validation | Commands match the CLI as built. Error texts for the API engine layer are planned, not yet executed. |

All commands run as root from the project directory (or with `-f` and
`--project-directory`). The full command list is in the
[README](../../README.md#commands) and the file format in
[compose-spec](../compose-spec.md).

## 1. Daily operation

| Task | Command |
|---|---|
| Validate the file before applying | `gantry config` (or `--pods`, `--services`, `--json`) |
| Start or apply changes | `gantry up -d` (add `--wait` to wait for health) |
| Status | `gantry ps`, `gantry pod ls` |
| Pod detail, including leases | `gantry pod inspect <pod>` |
| Logs | `gantry logs -f --tail 100 [svc...]` |
| Shell in a service | `gantry exec <svc> sh` |
| Restart services, keep pods | `gantry restart [svc...]` |
| Stop services, keep pods and leases | `gantry stop [svc...]` |
| Update images | `gantry pull && gantry up -d` |
| Build images for services with `build:` | `gantry build [svc...]`, or `gantry up -d --build` |
| Remove everything | `gantry down` (`-v` also removes named volumes) |

`up` is idempotent. A second run with no changes reports each container as
up to date. Changing a pod's addresses, networks, hostname, shared
namespaces or sysctls recreates that pod and its containers. Changing only
firewall rules or other pods' addresses is applied live.

### Inspecting a pod's network

Gantry pods are ordinary named network namespaces.

```sh
ip netns list | grep '^gt-'                         # gt-<project>-<pod>
gantry pod exec web ip addr
gantry pod exec web ip route
gantry pod exec web tcpdump -ni eth0 port 67 or port 68
ip -n gt-<project>-web neigh
```

### Firewall

```sh
gantry firewall show                # every pod: loaded rules with hit counts
gantry firewall show web --raw      # one pod: generated ruleset and nft listing
gantry pod exec web nft list ruleset
gantry firewall reload              # re-render every pod's firewall and hosts file
```

The host's own ruleset never contains a `gantry` table. With
`log_dropped: true`, drops are logged with the prefix
`gantry <pod> in drop:`; read them with `journalctl -k | grep gantry`.

### DHCP agents

Each DHCP interface has an agent process inside the pod namespace.

```sh
ps -ef | grep 'gantry internal dhcp-agent'
cat /run/gantry/<project>/pods/<pod>/dhcp-eth0.json     # current lease
tail -f /run/gantry/<project>/pods/<pod>/dhcp-eth0.log  # agent log
```

The log shows `lease bound` with address, server and lease time, plus
renewals, rebinds and errors. Raise detail with `GANTRY_LOG=debug` on the
`up` that starts the agent.

### Engines

Gantry uses each engine's API socket. Check them directly:

```sh
curl -s --unix-socket /var/run/docker.sock http://d/_ping     # Docker
curl -s --unix-socket /run/kiri/docker.sock http://d/_ping    # kiri
```

Containers carry `io.gantry.*` labels (project, service, pod, pod-id,
config-hash), so any Docker API client can filter on them.

### Moving data

Trigger: a service's data has to live somewhere else, for example from
local disk to NFS, or out of a named volume onto a host path.

The project file is the source of truth, so change it first and let
`data move` make the data follow. Gantry runs rsync (the package depends on
it).

1. Change the path. Usually that is one variable in the project's `.env`
   (`DATA_DIR=/new/path`). To leave a named volume, change `db:/data` to a
   host path in the project file. The running containers keep using the old
   location until the move.
2. Check the plan:

   ```sh
   sudo gantry data move --dry-run
   ```

   It lists every mount whose running source differs from the file, and the
   services that use it.
3. Move:

   ```sh
   sudo gantry data move
   ```

   * Up to three rsync passes copy the data while the services run
     (`--passes N` to change).
   * Only the services that use the data are stopped. A final pass copies
     what changed, and a checksum dry run must find no differences.
   * The services are recreated on the new path.

   Downtime is the final pass plus a restart. A destination that already
   holds data is refused unless `--force` is given. If the destination
   filesystem has no ACLs or extended attributes, rsync fails with
   "Operation not supported"; rerun with `--no-xattrs`.
4. Check the service, then remove the old data yourself. `data move` never
   deletes it.

Failure handling: if a pass or the verification fails after the services
stopped, Gantry restarts them on the old data and leaves a `.gantry-move`
marker in the destination. `up` refuses to start a service on a destination
with that marker, or on an empty destination while the old path has data,
so a half-copied path is never used. Fix the cause and rerun `data move`,
which continues where it stopped.

## 2. Troubleshooting

### 2.1 Common errors

| Message or symptom | Cause | Action |
|---|---|---|
| `gantry runs rootful; run it as root` | Not root. | Use `sudo`. |
| `engine: ... /var/run/docker.sock ...` (connection refused or not found) | dockerd not running. | `systemctl start docker`. |
| `engine: ... /run/kiri/docker.sock ...` | kiri not running, or listening elsewhere. | Start kiri; set `GANTRY_KIRI_HOST` if needed. |
| Docker reports `unknown or invalid runtime name: gantry` | Shim not registered. | `gantry setup docker`. |
| `gantry: pod network namespace ... does not exist; run gantry up again` (from Docker) | Docker restarted a container whose pod is gone (often after a reboot). | `gantry up -d`. |
| `gantry: network namespace path ... is outside ...` | A container was created outside Gantry with Gantry annotations. | Not a Gantry-managed container; remove the annotations. |
| kiri: `network ns:/run/netns/... not found` | Stock kiri without the patch. | Install the patched kiri (`contrib/kiri`). |
| `config: ... driver bridge ...` | Only macvlan and ipvlan are supported. | Use a macvlan or ipvlan network with a parent. |
| `config: ... needs an address` | Pod network has no static address and no `dhcp: true`. | Add one; Gantry does not allocate LAN addresses. |
| Build fails with the `docker` CLI not found | Builds use the Docker CLI, for Docker and kiri. | Install the `docker` CLI, or build elsewhere and use `image:`. |
| Pull fails with an authentication error | No credentials for the registry in the Docker auth file. | Log in as root (`docker login`), or configure a credential helper. |
| `config: ... io.gantry. ...` | Reserved label in the file. | Rename the label. |
| `network: ... File exists` or `Device or resource busy` on link creation | Parent already carries the other link type (macvlan vs ipvlan), or a duplicate MAC. | Use one link type per parent; check `mac_address` values. |
| `firewall: ... has no address` | A deny rule names a pod with no address yet (DHCP not bound). | Wait for the lease and run `gantry firewall reload`, or use a CIDR. |
| `... has no healthcheck, so service_healthy can never be met` | `depends_on` condition needs a healthcheck. | Add a healthcheck or use `service_started`. |
| `timed out waiting for <svc>` | Dependency not healthy within the timeout. | Check `gantry logs <svc>`; raise the wait timeout. |
| `image ... is not present and pull_policy is never` | Image missing. | `gantry pull` or change the policy. |
| Pod has no address with `dhcp: true` | No lease yet. | See 2.3. |

| `config: unknown runtime "podman"` | Podman support was removed in 0.1.0. | Use `runtime: docker` (or `kiri`) and `gantry up -d`. Old Podman containers are not seen by Gantry; remove them with `podman rm -f`. |
| A service stays `unhealthy` and is not restarted | It does not set `health_on_failure`, the watcher is not running, or it gave up after 5 restarts in 30 minutes. | `journalctl -u gantry-supervise` shows each action and `gave up`. Fix the service; `gantry up -d --force-recreate SVC` starts it with a clean count. |
| `another health watcher is running on this host` | `gantry supervise` was started while `gantry-supervise.service` or gantry-agent holds `/run/gantry/heal.lock`. | Expected; only one watcher runs per host. |

### 2.1a Projects that collide

| Error | Cause | Fix |
|---|---|---|
| `project NAME is already running from DIR; this one is in ...` | Two project directories use the same `name:`. Gantry keys pods and state by name, so it will not let the second touch the first's pods. | Give one of them a different `name:`, or `down` the running one first. |
| `pod P: address A is in use by pod Q of project R on this host` | Two projects were given the same static address. | Reserve another address in Nautobot and change one of them. |

### 2.2 Pod not reachable from the LAN

1. Confirm the address: `gantry pod exec <pod> ip addr`.
2. Confirm the route: `gantry pod exec <pod> ip route` shows
   `default via <gateway>`.
3. Check the firewall: `gantry firewall show <pod>`. A rising count on
   a reject rule or the policy row shows what is being turned away. Without
   `firewall:` the pod is open; with it, input is dropped unless a rule,
   `ports:` or `expose:` opens the port.
4. Test from another machine, not the host. **The host cannot reach its own
   macvlan pods through the parent interface**; that is how macvlan works.
5. Watch traffic: `gantry pod exec <pod> tcpdump -ni eth0`.
6. Stale ARP on the router after reusing an address: Gantry sends
   gratuitous ARP on every assignment. If a device still holds the old MAC,
   clear its ARP entry.

### 2.3 DHCP problems

1. Read the agent log (`dhcp-<if>.log` in the pod directory).
2. Watch the exchange: `gantry pod exec <pod> tcpdump -ni eth0 port 67 or port 68`.
3. No offers on ipvlan: ipvlan links share the parent's MAC, so the DHCP
   server must key leases on the client identifier (Gantry always sends
   `gantry-<project>-<pod>-<if>`).
4. Offers seen but not accepted: check the log for rejected masks or
   addresses. Lease values are validated.
5. After an address change, other pods pick up the new address
   automatically. If they did not, run `gantry firewall reload`.

### 2.4 Name resolution

* Inside a pod, sibling services resolve to `127.0.0.1`; other pods to their
  LAN address. Check with `gantry exec <svc> cat /etc/hosts`.
* DNS servers come from `dns:`, else the DHCP lease, else the host's upstream
  servers (loopback resolvers such as 127.0.0.53 are skipped). Check
  `/etc/resolv.conf` in the container.

### 2.5 Problems that are not Gantry

Some networks filter traffic between segments, reset plain HTTP while
allowing HTTPS, or only allow DNS to specific resolvers. Before treating a
connectivity problem as a Gantry fault, reproduce it without Gantry:

```sh
ip netns add probe
ip link add probe0 link <parent> type macvlan mode bridge
ip link set probe0 netns probe
ip -n probe link set lo up
ip -n probe link set probe0 up
ip -n probe addr add <free address>/<prefix> dev probe0
ip -n probe route add default via <gateway>
ip netns exec probe curl -v http://<target>/
ip netns del probe
```

If the plain namespace shows the same behavior, the cause is on the
network path.

## 3. Collecting diagnostics

When reporting a problem, collect:

```sh
gantry --version
uname -r
gantry config --json > config.json
gantry ps; gantry pod ls
gantry pod inspect <pod>
gantry firewall show <pod> --raw
GANTRY_LOG=debug gantry up -d 2> up-debug.log
ls -lR /run/gantry/<project>
```

Remove addresses and names that should not leave the site before sharing.
