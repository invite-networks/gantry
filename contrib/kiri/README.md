# kiri patch: `--network ns:<path>`

`0001-network-mode-ns-path.patch` adds a Podman-style `ns:<path>` network mode
to the [kiri](https://github.com/libracoder/kiri) container engine. kirisame
creates each pod's network namespace itself (interface, address, routes,
nftables) and needs Docker, Podman and kiri containers to join it without an
infra container. Docker and Podman can already do that; stock kiri 0.2.x
answers `network ns:/run/netns/<pod> not found`.

Base: upstream `b249ecc` (Release notes for 0.2.1).

## What it does

- The daemon accepts `HostConfig.NetworkMode = "ns:<absolute path>"`, for
  example `/run/netns/<pod>` as made by `ip netns add <pod>`.
- The container joins that namespace as it is. Kiri creates no bridge, veth,
  address, DNS listener or port relay, and attaches no extra networks.
- Teardown never deletes or changes anything in that namespace. Stopping,
  restarting or removing a container leaves the interface, address and routes
  untouched.
- The container keeps its own hostname, `/etc/hosts` and `/etc/resolv.conf`
  (same as Podman). `--hostname`, `--dns`, `--dns-search` and `--add-host`
  work. Without `--dns`, resolv.conf gets the host's upstream resolvers.
- Refused at create: a relative path, `-p`/`-P`, `--mac-address`, `--link`.
- A missing path or a file that is not a network namespace fails at start
  with a clear error (`network namespace /run/netns/x: not a network
  namespace`).
- `docker inspect` shows `NetworkMode: ns:<path>` and empty
  `NetworkSettings.Networks`, as for `container:` mode.
- `docker exec`, `docker restart` and restart policies work as usual.

Code changes:

| File | Change |
| --- | --- |
| `crates/kiri-proto/src/lib.rs` | New net option key `netns_path` |
| `crates/kiri/src/daemon/api.rs` | Accept and validate `ns:` at create, empty endpoints in inspect |
| `crates/kiri/src/daemon/engine.rs` | Pass `netns_path` to the agent, `shares_netns()` helper for ports, connect and listening |
| `crates/kiri-init/src/linux/container.rs` | Open the path, check it with `NS_GET_NSTYPE`, join it like `container:` mode |

Unit tests are added next to the existing network mode tests in all three
places.

## Build and install on a Linux host

Tested on Debian 13 (x86_64) with Rust 1.100 and the kiri-lite 0.2.0 `.deb`
installed. The daemon finds `kiri-init` next to its own binary, so both go
into the same directory.

```sh
git clone https://github.com/libracoder/kiri.git
cd kiri
git checkout b249ecc
git am path/to/0001-network-mode-ns-path.patch

cargo build --release -j 2 -p kiri -p kiri-init
cargo test --release -j 2 -p kiri -p kiri-init -- \
  only_a_network_namespace_path_is_joined a_namespace_path_reaches_the_guest \
  ns_network_mode_takes container_network_mode_refuses

sudo install -m 0755 target/release/kiri      /usr/local/bin/kiri
sudo install -m 0755 target/release/kiri-init /usr/local/bin/kiri-init
```

Point the packaged `kiri.service` at the patched build with a drop-in, which
leaves `/usr/bin/kiri` from the package untouched:

```sh
sudo mkdir -p /etc/systemd/system/kiri.service.d
sudo tee /etc/systemd/system/kiri.service.d/kirisame.conf <<'EOF'
# kirisame: run the kiri build patched for --network ns:<path>
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

```sh
sudo ip netns add kp
sudo ip link add kp0 link vlan4 type macvlan mode bridge
sudo ip link set kp0 netns kp
sudo ip -n kp link set lo up
sudo ip -n kp link set kp0 name eth0 up
sudo ip -n kp addr add <ip>/<prefix> dev eth0
sudo ip -n kp route add default via <gateway>

docker -H unix:///run/kiri/docker.sock run --rm --network ns:/run/netns/kp \
  alpine sh -c 'ip addr; ping -c1 -W2 <gateway>'
```

## Notes for kirisame

- Pass `--hostname <pod>` (or `Hostname` in the API) for each container if
  the pod should share a hostname. Kiri does not share hostname or /etc files
  between containers in an `ns:` namespace, which differs from
  `container:` mode.
- Pass `--dns` if the pod needs specific resolvers. The default is the host's
  upstream resolvers, which may not be reachable from the pod's network.
- `/etc/hosts` contains `host.docker.internal` pointing at kiri's internal
  host address (198.19.248.254), which is not reachable from a pod namespace.
  Use `--add-host` for anything the pod needs.
- Sysctls: kiri does not apply its usual per-namespace defaults
  (`ip_unprivileged_port_start`, `ping_group_range`) in an `ns:` namespace.
  Set them on the namespace when creating it if needed. Explicit
  `--sysctl net.*` values are written into the shared namespace.
- Kiri only uses the namespace. Creating and deleting it, and its interfaces
  and firewall rules, remain kirisame's job.

## Upstream PR

Would be opened against `libracoder/kiri` as:

**Title:** Network mode `ns:<path>`: join an existing network namespace

**Body:** Adds Podman's `--network ns:<path>` so a container can join a
network namespace created outside kiri (for example `ip netns add`), as pod
managers and CNI-style tools need. The daemon validates the path and refuses
options that would need kiri to change the namespace (ports, MAC, links); the
agent checks the path is a network namespace and joins it the way
`container:` mode joins another container's. Nothing in the namespace is
created or removed. Linux host engine only; in the macOS VM the path would
refer to the guest. Unit tests included; tested end to end on Debian 13 with
two containers sharing loopback, `docker exec`, restart and daemon restart
with a restart policy.

Not pushed and no PR opened.
