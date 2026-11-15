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
gantry firewall show web            # generated ruleset and what is loaded
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
curl -s --unix-socket /run/podman/podman.sock http://d/_ping  # Podman
curl -s --unix-socket /run/kiri/docker.sock http://d/_ping    # kiri
```

Containers carry `io.gantry.*` labels (project, service, pod, pod-id,
config-hash), so any Docker API client can filter on them.

## 2. Troubleshooting

### 2.1 Common errors

| Message or symptom | Cause | Action |
|---|---|---|
| `gantry runs rootful; run it as root` | Not root. | Use `sudo`. |
| `engine: ... /var/run/docker.sock ...` (connection refused or not found) | dockerd not running. | `systemctl start docker`. |
| `engine: ... /run/podman/podman.sock ...` | Socket not answering, `podman.socket` could not be started, and the private service could not start. | `gantry setup podman`; check `systemctl status podman.socket`; check that `podman` is installed for the fallback. |
| `engine: ... /run/kiri/docker.sock ...` | kiri not running, or listening elsewhere. | Start kiri; set `GANTRY_KIRI_HOST` if needed. |
| Docker reports `unknown or invalid runtime name: gantry` | Shim not registered. | `gantry setup docker`. |
| `gantry: pod network namespace ... does not exist; run gantry up again` (from Docker) | Docker restarted a container whose pod is gone (often after a reboot). | `gantry up -d`. |
| `gantry: network namespace path ... is outside ...` | A container was created outside Gantry with Gantry annotations. | Not a Gantry-managed container; remove the annotations. |
| kiri: `network ns:/run/netns/... not found` | Stock kiri without the patch. | Install the patched kiri (`contrib/kiri`). |
| `config: ... driver bridge ...` | Only macvlan and ipvlan are supported. | Use a macvlan or ipvlan network with a parent. |
| `config: ... needs an address` | Pod network has no static address and no `dhcp: true`. | Add one; Gantry does not allocate LAN addresses. |
| Build fails with the `docker` or `podman` CLI not found | Builds use the engine CLI. | Install the CLI for that runtime, or build elsewhere and use `image:`. |
| Pull fails with an authentication error | No credentials for the registry in the Docker or Podman auth files. | Log in as root (`docker login` or `podman login`), or configure a credential helper. |
| `config: ... io.gantry. ...` | Reserved label in the file. | Rename the label. |
| `network: ... File exists` or `Device or resource busy` on link creation | Parent already carries the other link type (macvlan vs ipvlan), or a duplicate MAC. | Use one link type per parent; check `mac_address` values. |
| `firewall: ... has no address` | A deny rule names a pod with no address yet (DHCP not bound). | Wait for the lease and run `gantry firewall reload`, or use a CIDR. |
| `... has no healthcheck, so service_healthy can never be met` | `depends_on` condition needs a healthcheck. | Add a healthcheck or use `service_started`. |
| `timed out waiting for <svc>` | Dependency not healthy within the timeout. | Check `gantry logs <svc>`; raise the wait timeout. |
| `image ... is not present and pull_policy is never` | Image missing. | `gantry pull` or change the policy. |
| Pod has no address with `dhcp: true` | No lease yet. | See 2.3. |

### 2.2 Pod not reachable from the LAN

1. Confirm the address: `gantry pod exec <pod> ip addr`.
2. Confirm the route: `gantry pod exec <pod> ip route` shows
   `default via <gateway>`.
3. Check the firewall: `gantry firewall show <pod>`. Without
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
gantry firewall show <pod>
GANTRY_LOG=debug gantry up -d 2> up-debug.log
ls -lR /run/gantry/<project>
```

Remove addresses and names that should not leave the site before sharing.
