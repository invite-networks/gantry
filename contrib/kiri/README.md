# kiri patches: `ns:<path>` for network, UTS and IPC

One patch adds Podman-style `ns:<path>` namespace modes to the
[kiri](https://github.com/libracoder/kiri) container engine. Gantry pins each
pod's network, UTS and IPC namespaces as files (`/run/netns/gt-<project>-<pod>`
and `/run/gantry/<project>/pods/<pod>/{uts,ipc}`) and needs Docker, Podman and
kiri containers to join them by path, without an infra container. Docker and
Podman can already do that; stock kiri 0.2.x answers `network
ns:/run/netns/<pod> not found` and ignores `UTSMode` and `IpcMode`.

| Patch | Adds |
| --- | --- |
| `0001-ns-path-network-uts-ipc.patch` | `HostConfig.NetworkMode`, `UTSMode` and `IpcMode = "ns:<path>"` |

Base: upstream `193dfd7` (Engine: always and unless-stopped containers come
back after the VM crashes, #267). Earlier this was two patches on `b249ecc`;
they were rebased, combined into one change and formatted for the upstream
pull request.

## Network

- The daemon accepts `HostConfig.NetworkMode = "ns:<absolute path>"`, for
  example `/run/netns/<pod>` as made by `ip netns add <pod>`.
- The container joins that namespace as it is. Kiri creates no bridge, veth,
  address, DNS listener or port relay, and attaches no extra networks.
- Teardown never deletes or changes anything in that namespace. Stopping,
  restarting or removing a container leaves the interface, address and routes
  untouched.
- The container keeps its own `/etc/hosts` and `/etc/resolv.conf` (same as
  Podman). `--dns`, `--dns-search` and `--add-host` work. Without `--dns`,
  resolv.conf gets the host's upstream resolvers.
- Refused at create: a relative path, `-p`/`-P`, `--mac-address`, `--link`.
- A missing path or a file that is not a network namespace fails at start
  with a clear error (`network namespace /run/netns/x: not a network
  namespace`). Anything that is not a regular file (a FIFO, a device, a
  socket) is refused before it is opened (`not a namespace file`), and the
  open itself is non-blocking, so such a path cannot hang the agent.
- `docker inspect` shows `NetworkMode: ns:<path>` and empty
  `NetworkSettings.Networks`, as for `container:` mode.
- `docker exec`, `docker restart` and restart policies work as usual.

## UTS and IPC

- The daemon accepts `HostConfig.UTSMode = "ns:<absolute path>"` and
  `HostConfig.IpcMode = "ns:<absolute path>"`, independently of each other and
  of the network mode. A relative path is refused at create (`Invalid UTSMode:
  invalid namespace format ns:<absolute path>`). Other modes (`host`,
  `container:`, `shareable`) are ignored as before, giving the container
  namespaces of its own.
- The agent checks each path with `NS_GET_NSTYPE` and the child joins it
  instead of cloning a new UTS or IPC namespace. A wrong file fails at start
  (`IPC namespace /run/x/uts: not an IPC namespace`). Nothing in the
  namespaces is created, changed or removed.
- Hostname: in a joined UTS namespace the hostname is the namespace's. Kiri
  never calls `sethostname` or `setdomainname` there. It reads the
  namespace's hostname and writes it to `$HOSTNAME`, `/etc/hostname` and the
  container's own line in `/etc/hosts`. `Hostname` and `Domainname` from the
  client are ignored without an error (Podman refuses them; ignoring keeps
  Docker clients that always send them working).
- IPC: the child joins the IPC namespace before mounting `/dev/mqueue`, so
  System V IPC and POSIX message queues are the pod's.
- `docker inspect` shows `HostConfig.UTSMode` and `HostConfig.IpcMode`
  (`private` when not set).
- `docker exec` and `docker restart` land in the same namespaces.

Code changes:

| File | Change |
| --- | --- |
| `crates/kiri-proto/src/lib.rs` | New option keys `netns_path`, `utsns_path`, `ipcns_path` |
| `crates/kiri/src/daemon/api.rs` | Accept and validate `ns:` modes at create, inspect output |
| `crates/kiri/src/daemon/engine.rs` | `uts_mode` and `ipc_mode` in the container spec, pass paths to the agent, `shares_netns()` helper |
| `crates/kiri-init/src/linux/container.rs` | `open_ns_path()` checks each path's type, the child joins instead of cloning, hostname read from the joined UTS namespace |

Unit tests sit next to the existing network mode tests in all three crates.

## Build and install on a Linux host

Tested on Debian 13 (x86_64) with Rust 1.100 and the kiri-lite 0.2.0 `.deb`
installed. The daemon finds `kiri-init` next to its own binary, so both go
into the same directory.

```sh
git clone https://github.com/libracoder/kiri.git
cd kiri
git checkout 193dfd7
git am path/to/0001-ns-path-network-uts-ipc.patch

cargo build --release -j 2 -p kiri -p kiri-init
cargo test --release -j 2 -p kiri -p kiri-init -- \
  only_a_network_namespace_path_is_joined only_a_uts_or_ipc_namespace_path_is_joined \
  a_namespace_path_reaches_the_guest uts_and_ipc ns_network_mode_takes \
  container_network_mode_refuses

sudo install -m 0755 target/release/kiri      /usr/local/bin/kiri
sudo install -m 0755 target/release/kiri-init /usr/local/bin/kiri-init
```

Point the packaged `kiri.service` at the patched build with a drop-in, which
leaves `/usr/bin/kiri` from the package untouched:

```sh
sudo mkdir -p /etc/systemd/system/kiri.service.d
sudo tee /etc/systemd/system/kiri.service.d/gantry.conf <<'EOF'
# Gantry: run the kiri build patched for ns:<path> namespace modes
[Service]
ExecStart=
ExecStart=/usr/local/bin/kiri daemon
EOF
sudo systemctl daemon-reload
sudo systemctl restart kiri
```

To go back to the packaged build, remove the drop-in, run
`systemctl daemon-reload` and restart `kiri`.

## Try it

The Docker CLI checks `--uts` and `--ipc` itself and rejects `ns:` (`--uts:
invalid UTS mode`) before the request reaches kiri, so UTS and IPC go through
the API. `--network ns:` works from the CLI.

```sh
sudo ip netns add kp
sudo ip -n kp link set lo up
sudo mkdir -p /run/kp && sudo touch /run/kp/uts /run/kp/ipc
sudo unshare --uts=/run/kp/uts --ipc=/run/kp/ipc hostname kp-pod

curl -s --unix-socket /run/kiri/docker.sock -H 'Content-Type: application/json' \
  -X POST 'http://x/containers/create?name=kp-a' -d '{
    "Image": "alpine:3",
    "Cmd": ["sh", "-c", "hostname; cat /etc/hostname; for n in uts ipc net; do readlink /proc/self/ns/$n; done"],
    "HostConfig": {"NetworkMode": "ns:/run/netns/kp",
                   "UTSMode": "ns:/run/kp/uts", "IpcMode": "ns:/run/kp/ipc"}}'
docker -H unix:///run/kiri/docker.sock start -a kp-a
```

For a routed network, move an interface into `kp` first (for example a
macvlan on `vlan4` with an address and default route) as Gantry does.

## Notes for Gantry

- Hostname: with `UTSMode` set, set the hostname on the pinned UTS namespace
  when creating it. Kiri takes it from there and ignores `Hostname`. Without
  `UTSMode`, pass `Hostname` per container.
- `/dev/shm` is not shared. POSIX shared memory lives in a filesystem, not in
  the IPC namespace, and each kiri container gets its own `/dev/shm` tmpfs.
  To share it, bind the same host directory at `/dev/shm` in every
  container of the pod.
- `/etc/hosts` and `/etc/resolv.conf` are per container. Pass `--dns` if the
  pod needs specific resolvers; the default is the host's upstream resolvers,
  which may not be reachable from the pod's network. `/etc/hosts` contains
  `host.docker.internal` pointing at kiri's internal host address
  (198.19.248.254), which is not reachable from a pod namespace. Use
  `--add-host` for anything the pod needs.
- Sysctls: kiri does not apply its usual per-namespace defaults
  (`ip_unprivileged_port_start`, `ping_group_range`) in an `ns:` network
  namespace. Set them on the namespace when creating it if needed. Explicit
  `--sysctl` values (`net.*`, `kernel.*` IPC keys, UTS keys) are written into
  the shared namespaces and so affect the whole pod.
- Kiri only uses the namespaces. Creating and deleting them, and the
  interfaces and firewall rules, remain Gantry's job.

## Upstream

Drafts of the upstream issue and pull request are in `UPSTREAM.md`.
Nothing has been pushed or opened.
