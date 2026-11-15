# kirisame

kirisame is a pod-first container orchestrator. It reads the Compose
format, creates each pod's namespaces itself, puts the pod straight onto
your LAN with macvlan or ipvlan, applies an nftables firewall inside the
pod, and then starts the pod's containers with Docker, Podman or kiri.

There is no infra or pause container. kirisame pins the namespaces the
same way `ip netns add` does, and every container joins them by path.

```
                 LAN (vlan4, 192.0.2.216/29)
                   |                        |
          macvlan 192.0.2.218      macvlan 192.0.2.219
        +----------+----------+   +---------+---------+
        | pod web  (netns, uts, ipc,    | pod data            |
        |          nftables)            |                     |
        |  nginx (docker)  <-127.0.0.1->|  redis (podman)     |
        |  sidecar (podman)             |                     |
        +-------------------------------+---------------------+
```

## Status

Version 0.1.0, rapid development. Tested on Debian 13 (kernel 6.12) with
Docker 29.9, Podman 5.4 and kiri 0.2.0. Rootful only.

## Install

kirisame needs Linux, root, `nft` (nftables) for pod firewalls, and at
least one engine.

```sh
cargo build --release
sudo install -m 755 target/release/kirisame target/release/kirisame-oci /usr/local/bin/
sudo kirisame setup docker      # once per host, if you use Docker
```

`setup docker` registers the `kirisame-oci` runtime shim in
`/etc/docker/daemon.json` and reloads dockerd. For kiri, see
`contrib/kiri`.

## A first pod

```yaml
# kirisame.yaml
name: demo

networks:
  lan:
    driver: macvlan
    driver_opts: { parent: eth0 }
    ipam:
      config:
        - subnet: 192.168.10.0/24
          gateway: 192.168.10.1

pods:
  web:
    networks:
      lan: { ipv4_address: 192.168.10.50 }
    firewall:
      input:
        - { proto: tcp, ports: [80, 443] }

services:
  nginx:
    image: nginx:alpine
    pod: web
  app:
    image: alpine:3
    pod: web
    runtime: podman
    command: ["sh", "-c", "while sleep 10; do wget -qO- http://localhost/ >/dev/null && echo ok; done"]
```

```sh
sudo kirisame up -d
sudo kirisame ps
sudo kirisame pod ls
sudo kirisame logs -f app
sudo kirisame down
```

`nginx` and `app` share the pod: one hostname, one address on the LAN, and
they talk over loopback. Other machines reach the pod at 192.168.10.50.

## Commands

| Command | What it does |
|---|---|
| `up [-d] [--wait] [--force-recreate] [--pull P] [svc...]` | Create pods, then create and start services in dependency order. Without `-d` it follows logs and stops services on Ctrl-C. |
| `down [-v] [-t SECS]` | Remove containers, then pods (namespaces, links, DHCP leases). `-v` also removes named volumes. |
| `ps` | Services, their pod, runtime and state. |
| `logs [-f] [--tail N] [svc...]` | Logs, prefixed by service. |
| `exec [-T] [-u U] [-w DIR] [-e K=V] svc cmd...` | Run a command in a service. |
| `start`, `stop`, `restart [svc...]` | Start or stop services; pods stay up. |
| `pull [svc...]` | Pull images. |
| `config [--services\|--pods\|--json]` | Validate and print the normalized project. |
| `pod ls` | Pods, their namespace, addresses and firewall state. |
| `pod inspect POD` | Pod state as JSON, including DHCP leases. |
| `pod exec POD cmd...` | Run a host command inside the pod's network namespace (`ip addr`, `tcpdump`, `nft list ruleset`). |
| `firewall show POD` | The generated ruleset and what is loaded. |
| `firewall reload` | Re-render hosts files and firewalls (after DHCP address changes). |
| `setup docker` | Register the Docker runtime shim. |

Global flags: `-f FILE` (repeatable), `-p NAME`, `--project-directory`,
`--env-file`, `--profile`, `--runtime docker|podman|kiri`, `--log-level`.

## How it works

1. **Pod namespaces.** For each pod kirisame creates a network namespace at
   `/run/netns/ks-<project>-<pod>` (so `ip netns` sees it), plus UTS and IPC
   namespaces under `/run/kirisame/<project>/pods/<pod>/`. It sets the
   hostname inside the UTS namespace.
2. **Links.** For every network it creates a macvlan or ipvlan link on the
   parent directly inside the pod namespace, renames it to `eth0`, `eth1`,
   ..., assigns static addresses and the default route, and sends
   gratuitous ARP. DHCP networks get a small agent process
   (`kirisame internal dhcp-agent`) that lives in the pod namespace, holds
   the lease, renews it and releases it on `down`.
3. **Shared files.** `/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname`
   are written to the pod directory and bind mounted into every container.
4. **Firewall.** The pod's rules are rendered into an `inet kirisame` table
   and loaded with `nft` inside the pod namespace.
5. **Containers.** Each service is created with its engine and joined to
   the pod's namespaces by path. Labels (`io.kirisame.*`) record the project,
   service, pod instance and a config hash, so a second `up` only touches
   what changed. A pod whose network shape changes is recreated along with
   its containers.

### Docker and the runtime shim

The Docker CLI and API cannot join a namespace by path. kirisame starts
Docker containers with `--runtime kirisame --network none` and annotations
naming the pod's namespaces. Docker passes annotations into the OCI spec;
`kirisame-oci` rewrites the namespace paths in `config.json` (and drops the
hostname when the UTS namespace is shared) and execs `runc` with the
original arguments. Set `KIRISAME_OCI_RUNTIME` in dockerd's environment, or
the `io.kirisame.oci-runtime` annotation, to hand off to another runtime.

### kiri

kiri speaks the Docker Engine API, so kirisame drives it with
`docker -H unix:///run/kiri/docker.sock` (override with
`KIRISAME_KIRI_HOST`). Upstream kiri 0.2.0 only joins another container's
namespace (`container:<name>`). The patch in `contrib/kiri` adds Podman's
`ns:<path>` network mode. kiri containers share the pod's network
namespace; they keep their own UTS and IPC namespaces, and kirisame sets
the pod hostname on them.

## Things to know

* **The host cannot reach its own macvlan pods** through the parent
  interface. That is how macvlan works. Reach them from another machine or
  through the router, or use a parent the host does not use itself.
* A parent interface carries macvlan or ipvlan links, not both.
* ipvlan links share the parent's MAC address, so DHCP servers must key
  leases on the client identifier (kirisame always sends one).
* Pods rely on the LAN for DNS. Without `dns:` a static pod uses the host's
  upstream servers (skipping loopback resolvers such as systemd-resolved's
  127.0.0.53, which a pod cannot reach).
* Docker-only state such as restart policies keeps working: when Docker
  restarts a container it runs the shim again and rejoins the pod.

## Environment

| Variable | Default | Use |
|---|---|---|
| `KIRISAME_RUN_DIR` | `/run/kirisame` | State directory |
| `KIRISAME_NETNS_DIR` | `/run/netns` | Where network namespaces are pinned |
| `KIRISAME_DOCKER` | `docker` | Docker CLI (also used for kiri) |
| `KIRISAME_PODMAN` | `podman` | Podman CLI |
| `KIRISAME_KIRI_HOST` | `unix:///run/kiri/docker.sock` | kiri's API socket |
| `KIRISAME_NFT` | `nft` | nftables CLI |
| `KIRISAME_LOG` | `info` | Log level |

## Development

```sh
cargo test                                   # unit tests (Linux)
KIRISAME_DEV_HOST=user@host scripts/remote.sh cargo test   # from a Mac
sudo tests/integration/run.sh                # live tests; see the script header
```

See [docs/compose-spec.md](docs/compose-spec.md) for the full file format
and [DECISIONS.md](DECISIONS.md) for design choices.

## License

MIT
