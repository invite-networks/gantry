# The kirisame file format

kirisame reads the Compose format and extends it where pods need it. A plain
`compose.yaml` works when its services set `image:` and use macvlan or
ipvlan networks. Everything below is either standard Compose or marked as a
kirisame extension.

Files are looked up in this order: `kirisame.yaml`, `kirisame.yml`,
`compose.yaml`, `compose.yml`, `docker-compose.yaml`, `docker-compose.yml`.
A matching `*.override.yaml` next to it is merged on top. `-f` takes any
number of files, merged left to right. Mappings merge recursively. Lists
under `ports`, `expose`, `volumes`, `devices`, `cap_add`, `cap_drop`, `dns`,
`dns_search`, `tmpfs`, `env_file`, `profiles`, `security_opt`, `group_add`
and firewall `input`/`output` are concatenated. Every other value is
replaced.

Variables (`$VAR`, `${VAR}`, `${VAR:-default}`, `${VAR-default}`,
`${VAR:?error}`, `${VAR?error}`, `${VAR:+alt}`, `${VAR+alt}`, `$$`) come
from the environment first, then from `.env` in the project directory (or
`--env-file`). YAML anchors and `<<` merge keys work.

## Top level

| Key | Meaning |
|---|---|
| `name` | Project name. Default: `-p`, `COMPOSE_PROJECT_NAME`, then the directory name. |
| `runtime` | **Extension.** Default runtime for services: `docker` (default), `podman` or `kiri`. |
| `networks` | LAN segments pods attach to (see below). |
| `pods` | **Extension.** The pods. |
| `services` | Containers. Each runs in exactly one pod. |
| `volumes` | Named volumes, created in the service's engine as `<project>_<name>`. |
| `x-*` | Ignored, for anchors. |

`--runtime` on the command line forces every service onto one runtime.

## networks

kirisame does not build bridges or NAT. A network is a parent interface on
the host, and each pod gets its own macvlan or ipvlan link on it with its own
address on the LAN.

```yaml
networks:
  lan:
    driver: macvlan          # macvlan (default) or ipvlan
    driver_opts:
      parent: vlan4          # required; also accepted as `parent:` directly
      mode: bridge           # macvlan: bridge, private, vepa, passthru; ipvlan: l2, l3, l3s
      mtu: 1500              # optional; also accepted as `mtu:` directly
    ipam:
      config:
        - subnet: 192.0.2.216/29
          gateway: 192.0.2.222
```

The `subnet` gives bare addresses their prefix length. The `gateway` becomes
the default route of pods on this network.

A parent can carry macvlan links or ipvlan links, not both at once (a kernel
rule).

## pods (extension)

```yaml
pods:
  web:
    hostname: web               # default: the pod name
    domainname: example.com     # optional
    share: [uts, ipc]           # default; net is always shared
    ipv6: true                  # false disables IPv6 in the pod
    runtime: podman             # default runtime for services in this pod
    networks:
      lan:
        ipv4_address: 192.0.2.218        # bare address takes the subnet prefix
        # ipv6_address: 2001:db8::10/64
        # addresses: [10.0.0.5/24]        # extension: any number, with prefix
        # dhcp: true                      # extension: lease IPv4 with DHCP
        # mac_address: 02:00:00:00:00:10  # macvlan only; default is a stable MAC
        # default_route: true             # extension: which network carries the default route
        # gateway: 192.0.2.222           # extension: override the subnet gateway
        # interface: eth0                 # extension: name inside the pod (eth0, eth1, ...)
    dns: [1.1.1.1]              # default: the lease's servers, else the host's upstream servers
    dns_search: [example.com]
    dns_opt: [ndots:2]
    extra_hosts: ["gw:192.0.2.222"]
    sysctls:                    # net.* only; applied to the pod namespace
      net.ipv4.tcp_keepalive_time: "600"
    firewall: { ... }           # see below
```

Every pod network needs an address: `ipv4_address`, `addresses` or
`dhcp: true`. kirisame never picks LAN addresses on its own.

With macvlan, a pod's MAC is derived from the project, pod and network names,
so it keeps the same MAC (and DHCP lease) across restarts. kirisame sends
gratuitous ARP when it assigns an address so the LAN forgets stale entries.

The first network carries the default route unless another sets
`default_route: true`.

### What the pod shares

All services in a pod share:

* the network namespace (interfaces, addresses, routes, firewall, ports).
  Services reach each other on `127.0.0.1`/`localhost`.
* the UTS namespace (hostname) and IPC namespace, unless `share` says
  otherwise. (kiri containers keep their own UTS and IPC namespaces; see
  the README.)
* `/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname`, which kirisame
  writes and bind mounts read only.

### Names

`/etc/hosts` in every pod lists:

* the pod's hostname, pod name and its service names on `127.0.1.1` and
  `::1`, so `http://nginx/` from a sibling container stays on loopback;
* every other pod's first IPv4 and IPv6 address under its hostname, pod
  name and service names.

## firewall (extension)

A pod with a `firewall` section gets an nftables table (`inet kirisame`)
inside its own network namespace. The host's rules are never touched. A pod
without a `firewall` section is open.

```yaml
firewall:
  policy:
    input: drop        # default drop (accept, drop, reject)
    output: accept     # default accept
  allow_ping: true     # default true
  log_dropped: false   # log dropped packets (rate limited) with prefix "kirisame <pod> in drop:"
  input:
    - proto: tcp                 # tcp, udp, icmp, icmpv6, any, or a list
      port: 443                  # or ports: [80, 443, "8000-8100"]
      from: [198.51.100.0/24, pod:web, service:nginx, network:lan, any]
      action: accept             # default accept
      interface: eth0            # optional
      limit: 10/second           # optional
      log: false                 # optional
      comment: "https"           # optional
  output:
    - { to: 192.168.0.0/16, action: reject }
```

Always allowed, before your rules: loopback (intra-pod traffic), established
and related connections, IPv6 neighbor discovery, essential ICMP errors, and
DHCP replies on DHCP pods.

Peers:

| Peer | Matches |
|---|---|
| `any` or omitted | everyone |
| `10.0.0.0/8`, `2001:db8::/32`, `10.0.0.5` | that network or host |
| `pod:<name>` | that pod's current addresses (DHCP leases included) |
| `service:<name>` | the pod that service runs in |
| `network:<name>` | the network's subnets |

A rule with `port` and no `proto` is TCP.

Service `ports:` and `expose:` feed the firewall:

* `ports: ["80", "443/tcp"]` opens those ports to everyone. A host mapping
  like `"8080:80"` has no meaning for a pod on the LAN; kirisame opens the
  container port (80) and warns.
* `expose: ["5432"]` opens the port to the project's other pods only.

Rules that name pods are rendered from the addresses known at `up` time.
Run `kirisame firewall reload` after a DHCP pod's address changes.

## services

A service runs in the pod named by `pod:`. A service without `pod:` gets a
pod of its own, named after the service, built from the service's
`networks`, `hostname`, `domainname`, `mac_address`, `dns`, `dns_search`,
`dns_opt`, `extra_hosts`, `firewall` (extension) and `net.*` sysctls. That
keeps ordinary Compose files working. Setting those keys on a service that
also has `pod:` is an error: they belong to the pod.

`network_mode: service:<other>` (or `container:<other>`) puts the service in
the other service's pod. `network_mode: none` gives it a pod with loopback
only.

Supported keys: `image`, `pod`, `runtime` (extension), `container_name`,
`command`, `entrypoint`, `environment`, `env_file`, `working_dir`, `user`,
`labels`, `volumes` (bind, named, anonymous, tmpfs; short and long form),
`tmpfs`, `restart`, `healthcheck`, `depends_on` (`service_started`,
`service_healthy`, `service_completed_successfully`), `stop_signal`,
`stop_grace_period`, `cap_add`, `cap_drop`, `privileged`, `read_only`,
`devices`, `tty`, `stdin_open`, `init`, `mem_limit`, `cpus`, `pids_limit`,
`shm_size`, `ulimits`, `security_opt`, `group_add`, `sysctls` (non-net),
`pull_policy`, `ports`, `expose`, `profiles`.

Ignored with a warning: `logging`, `deploy`, `scale`, `links`,
`external_links`, `platform` and unknown keys. `build` is an error for now.

Container names default to `<project>-<service>-1`.

## Runtimes

| Runtime | Engine | How containers join the pod |
|---|---|---|
| `docker` | `docker` CLI | `--runtime kirisame --network none`, with annotations that the `kirisame-oci` shim turns into namespace paths before running runc. Run `kirisame setup docker` once per host. |
| `podman` | `podman` CLI | `--network ns:<path>`, `--uts ns:<path>`, `--ipc ns:<path>`. Bare image names are qualified to `docker.io`. |
| `kiri` | `docker -H unix:///run/kiri/docker.sock` | `--network ns:<path>`. Needs the kiri patch in `contrib/kiri`. |

Services in one pod may use different runtimes.
