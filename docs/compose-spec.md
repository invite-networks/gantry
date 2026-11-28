# The Gantry file format

Gantry reads the Compose format and extends it where pods need it. Services,
volumes and builds are standard Compose. Networks are not: each pod lists its
own interfaces, so a project has no top-level `networks:` section. Everything
below is either standard Compose or marked as a Gantry extension.

Files are looked up in this order: `gantry.yaml`, `gantry.yml`,
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
| `runtime` | **Extension.** Default runtime for services: `docker` (default) or `kiri`. |
| `pods` | **Extension.** The pods, each with its own network interfaces and firewall. |
| `pod` | **Extension.** The project's only pod, without a name: it takes the project's name (and so its hostname). Use `pod` or `pods`, not both. |
| `services` | Containers. Each runs in exactly one pod. |
| `volumes` | Named volumes, created in the service's engine as `<project>_<name>`. |
| `plugins` | **Extension.** Per project plugin settings, see [plugins](#plugins-extension). |
| `x-*` | Ignored, for anchors. |

`--runtime` on the command line forces every service onto one runtime.

A top-level `networks:` section is an error. Each pod defines its own
interfaces (see below).

## tls (extension)

A pod can have a certificate from an issuer plugin (such as gantry-stepca),
issued when it is created and refreshed before it runs out. Each service
that wants it says where its files go, what they hold, and what to do when
they change. The full design is [docs/spec/certificates.md](spec/certificates.md).

```yaml
pod:
  tls:
    names: [shop.infra.invite.global]   # required: the DNS names it carries
    key_type: ec256                     # ec256 (default), rsa2048, rsa4096
    optional: false                     # true: start without one and keep trying
    # issuer: stepca                    # when more than one issuer is installed

services:
  web:
    user: "1060:1060"                   # numeric user: owns the files by default
    tls:
      dir: /secrets                     # default /etc/tls, mounted read only
      files:                            # default: cert.pem chain.pem fullchain.pem key.pem ca.pem
        chain.pem: fullchain
        key.pem: key
        ca.der: { content: root, format: der }
        store.p12: { format: pkcs12, password: "${STORE_PASSWORD}" }
      # owner: "1060:1060"
      on_renew: { signal: SIGHUP }      # required: { signal }, { exec: [...] }, restart, none
```

| Content | Holds |
|---|---|
| `cert` | the certificate alone |
| `chain` | the intermediates |
| `fullchain` | certificate and intermediates |
| `key` | private key, PKCS#8 PEM |
| `key-pkcs1` | private key in the older RSA or EC form |
| `root` | the CA root |
| `bundle` | key and full chain in one file |

Files holding the key are mode 0400, the others 0444, owned by `owner:` or
the service's numeric `user:`. `der` is for one certificate or key, and
`pkcs12` (key and full chain) needs `password:`. A service with `tls:` must
be in a pod with `tls:`.

The files live in memory under `/run/gantry`, never with the project, so a
project moved to another host gets its certificate there. `gantry ps` shows
each pod's certificate and warns when one is missing or within 4 hours of
its end. `gantry-tls.service`, enabled by the package, refreshes
certificates and reports problems on every host, whatever runs the health
watcher. `gantry internal tls-watch --once` runs one pass by hand.

## plugins (extension)

Every plugin installed on the host runs for every project unless the
project opts out. A `plugins.<name>` block turns one off or passes it
settings:

```yaml
plugins:
  nautobot:
    enabled: false        # default true
    tenant: acme          # every other key goes to the plugin as settings
```

A block for a plugin that is not installed is ignored, so one file works on
hosts with and without it. Plugins are described in
[docs/spec/plugins.md](spec/plugins.md).

## pods (extension)

A project with one pod can skip naming it. A top-level `pod:` takes the
same keys as an entry under `pods:`, and the pod is named after the
project:

```yaml
name: shop
pod:
  network: [{ parent: vlan4, address: 192.0.2.218/29, gateway: 192.0.2.222 }]
services:
  web: { image: nginx }        # joins pod "shop"
  worker: { image: worker }
```

Pod names and addresses are per project. Two projects never share a pod,
even when their pods have the same name, and they reach each other over the
LAN. Gantry refuses to bring up a project whose static address another
project's pod on the host already holds.

```yaml
pods:
  web:
    network:
      - parent: vlan4                     # required: the host interface
        address: 192.0.2.218/29           # required: address with prefix, or dhcp
        # driver: ipvlan                  # macvlan (default) or ipvlan
        # gateway: 192.0.2.222            # default: first host of the subnet
        # address6: 2001:db8::10/64       # optional static IPv6 address
        # gateway6: fe80::1               # default: from router advertisements
        # interface: eth0                 # default: eth0, eth1, ... in order
        # default_route: true             # default: the first entry
        # mac_address: 02:00:00:00:00:10  # macvlan only; default is a stable MAC
    hostname: web               # default: the pod name
    domainname: example.com     # optional
    share: [uts, ipc]           # default; net is always shared
    ipv6: true                  # false disables IPv6 in the pod
    runtime: kiri               # default runtime for services in this pod
    priority: 100               # boot order across the host; lower first
    dns: [1.1.1.1]              # default: the lease's servers, else the host's upstream servers
    dns_search: [example.com]
    dns_opt: [ndots:2]
    extra_hosts: ["gw:192.0.2.222"]
    sysctls:                    # net.* only; applied to the pod namespace
      net.ipv4.tcp_keepalive_time: "600"
    firewall: { ... }           # see below
```

### network

`network` is a list. Each entry becomes one interface in the pod: a macvlan
or ipvlan link on `parent`, with its own address on the LAN. Gantry builds
no bridges and no NAT. A pod with two VLANs lists two entries.

* **parent** is the host interface, normally a VLAN interface such as
  `vlan4`. Required.
* **address** is required. Give an IPv4 address with its prefix length
  (`10.10.10.10/24`), `dhcp` to lease one from the LAN's DHCP server, or
  `{ from: <plugin>, ... }` to have an installed plugin allocate one when
  the pod is created (see [plugins](spec/plugins.md#addresses); the other
  keys go to the plugin). Gantry never picks an address on its own. An address without a prefix,
  or one that is its subnet's network, broadcast or gateway address, is an
  error.
* **gateway** defaults to the first host of the address's subnet:
  `10.10.10.1` for `10.10.10.10/24`. Set it when the gateway is elsewhere.
  A `/31` or `/32` has no first host, so it needs `gateway:`. DHCP entries
  take the gateway from the lease.
* **driver** is `macvlan` (the default) or `ipvlan`. macvlan links run in
  `bridge` mode, so pods on the same parent reach each other. ipvlan links
  run in `l2` mode and share the parent's MAC, for switch ports that allow
  one MAC. A parent carries macvlan links or ipvlan links, not both (a
  kernel rule), so a project that mixes them on one parent is an error.
* **address6** adds a static IPv6 address with prefix. Its gateway is
  `gateway6`, or the router advertisements on the LAN when that is omitted.
  `address6` together with `dhcp` is not supported yet.
* Links inherit the parent's MTU.

With macvlan, a pod's MAC is derived from the project, pod and interface
names, so it keeps the same MAC (and DHCP lease) across restarts. Gantry
sends gratuitous ARP when it assigns an address so the LAN forgets stale
entries.

The first entry carries the default route unless another sets
`default_route: true`.

### What the pod shares

All services in a pod share:

* the network namespace (interfaces, addresses, routes, firewall, ports).
  Services reach each other on `127.0.0.1`/`localhost`.
* the UTS namespace (hostname) and IPC namespace, unless `share` says
  otherwise.
* `/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname`, which Gantry
  writes and bind mounts read only.

### Names

`/etc/hosts` in every pod lists:

* the pod's hostname, pod name and its service names on `127.0.0.1`, so
  `http://nginx/` from a sibling container stays on loopback (IPv4 only, so
  servers bound to `127.0.0.1` or `0.0.0.0` are reached);
* every other pod's first IPv4 and IPv6 address under its hostname, pod
  name and service names.

## firewall (extension)

Every pod gets an nftables table (`inet gantry`) inside its own network
namespace, whether or not it has a `firewall` section. The host's rules are
never touched.

```yaml
firewall:
  policy:
    input: accept      # default accept (accept, drop, reject)
    output: accept     # default accept
  defaults: true       # default true; see below
  log_dropped: false   # log dropped packets (rate limited) with prefix "gantry <pod> in drop:"
  input:
    - proto: tcp                 # tcp, udp, icmp, icmpv6, any, or a list
      port: 443                  # or ports: [80, 443, "8000-8100"]
      from: [198.51.100.0/24, pod:web, service:nginx, subnet, any]
      action: accept             # default accept
      interface: eth0            # optional
      limit: 10/second           # optional
      log: false                 # optional
      comment: "https"           # optional
  output:
    - { to: 192.168.0.0/16, action: reject }
```

### The default policy

With `defaults: true`, which is the default, the input chain is:

1. Always allowed: loopback (intra-pod traffic), established and related
   connections, and DHCP replies on DHCP pods. Invalid packets are dropped.
2. ICMP, accepted, then ICMPv6, accepted.
3. The pod's own `input` rules, then the ports opened by `ports:` and
   `expose:`.
4. Everything from the pod's own subnet (`subnet`), rejected.
5. The input policy, `accept` unless set.

With `defaults: false` and an input policy of drop or reject, Gantry keeps
an accept for ICMP errors and ICMPv6 neighbor discovery in their place, so
path MTU discovery and IPv6 still work.

Every rule has a counter, and the last rule of each chain counts what falls
through to the policy. `gantry firewall show` prints them. Counters restart
from zero when the firewall is reloaded or the pod is recreated.

A pod with no `firewall` section gets exactly this. Neighbors on the same
VLAN cannot reach the pod, since that traffic never crosses a router and
nothing else would stop it. Traffic from other subnets is routed, and the
upstream firewall decides what reaches the pod.

To let a neighbor in, add a rule for it. Your rules come before the subnet
reject:

```yaml
firewall:
  input:
    - { proto: any, from: [192.0.2.219], comment: "VRRP peer" }
```

Set `defaults: false` to write the whole policy yourself. Steps 2 and 4 are
then left out, and step 1 still applies.

### Peers

| Peer | Matches |
|---|---|
| `any` or omitted | everyone |
| `subnet` | the subnets of the pod's own addresses: `10.10.10.0/24` for `10.10.10.10/24`. For DHCP entries, the subnet of the current lease. |
| `10.0.0.0/8`, `2001:db8::/32`, `10.0.0.5` | that network or host |
| `pod:<name>` | that pod's current addresses (DHCP leases included) |
| `service:<name>` | the pod that service runs in |

A rule with `port` and no `proto` is TCP.

Service `ports:` and `expose:` feed the firewall:

* `ports: ["80", "443/tcp"]` opens those ports to everyone, including the
  pod's own subnet. A host mapping like `"8080:80"` has no meaning for a
  pod on the LAN; Gantry opens the container port (80) and warns.
* `expose: ["5432"]` opens the port to the project's other pods only.

Rules that name pods, and `subnet`, are rendered from the addresses known
at the time. When a DHCP pod's address changes, its agent refreshes every
pod's hosts file and firewall; `gantry firewall reload` does the same by
hand. A deny rule whose pod has no address yet is an error rather than being
skipped. The default subnet reject on a DHCP pod waits for the first lease.

## services

A service runs in the pod named by `pod:`. When the project has exactly one
pod, from `pod:` or `pods:`, `pod:` on the service can be left out. With
more than one pod it is required, and a service without it is an error.
Settings that shape the pod (`networks`, `hostname`, `domainname`,
`mac_address`, `dns`, `dns_search`, `dns_opt`, `extra_hosts`, `firewall`,
`ipc`) are an error on a service: set them under `pods:`.

A service without `restart:` gets `restart: unless-stopped`, so its engine
restarts it after a crash. systemd only starts projects at boot
(`gantry-boot.service`); it does not watch containers. Write
`restart: "no"` (quoted) for a service that should stay down when it exits,
such as a one-shot migration.

`network_mode: service:<other>` (or `container:<other>`) puts the service in
the other service's pod. `network_mode: none` gives it a pod with loopback
only.

### priority (extension)

`priority` orders startup. Lower numbers start first, the default is 100,
and equal priorities start together.

* **On a pod**, it orders pods across every project registered for boot on
  the host (`gantry-boot.service`, `gantry boot ls`). Each level waits for
  its services to be running, or healthy where they have a healthcheck,
  before the next level starts.
* **On a service**, it orders services inside their pod. Gantry turns it
  into `depends_on` entries: a service waits for every service of its pod
  with a lower priority, `service_healthy` when that service has a
  healthcheck and `service_started` otherwise. An explicit `depends_on`
  keeps its own condition. A lower-priority service that depends on a
  higher-priority one in the same pod is an error. `depends_on` across pods
  is unaffected.

```yaml
pods:
  app:
    network: [{ parent: vlan4, address: 192.0.2.218/29, gateway: 192.0.2.222 }]
services:
  db:    { image: postgres:17, priority: 10, healthcheck: { test: [CMD, pg_isready] } }
  cache: { image: redis:7, priority: 10 }      # starts with db
  web:   { image: nginx }                      # 100: waits for db healthy, cache started
```

Supported keys: `image`, `pod`, `priority` (extension), `runtime` (extension), `container_name`,
`command`, `entrypoint`, `environment`, `env_file`, `working_dir`, `user`,
`labels`, `volumes` (bind, named, anonymous, tmpfs; short and long form),
`tmpfs`, `restart`, `healthcheck`, `health_on_failure` (extension, below), `depends_on` (`service_started`,
`service_healthy`, `service_completed_successfully`), `stop_signal`,
`stop_grace_period`, `cap_add`, `cap_drop`, `privileged`, `read_only`,
`devices`, `tty`, `stdin_open`, `init`, `mem_limit`, `cpus`, `pids_limit`,
`shm_size`, `ulimits`, `security_opt`, `group_add`, `sysctls` (non-net),
`pull_policy` (`missing`, `always`, `never`, `build`), `ports`, `expose`,
`profiles`, `logging` (`driver`, `options`), `platform`, and `build`
(below).

Ignored with a warning: `deploy`, `scale`, `links`, `external_links` and
unknown keys.

A service that sets no `cpus` gets 1 CPU, and one that sets no `mem_limit`
gets 2 GiB. Set `cpus: 0` or `mem_limit: 0` to run without that limit.

### health_on_failure

**Extension.** What happens when the service's container turns
`unhealthy`. Engines only mark it; `gantry supervise`
(`gantry-supervise.service`) acts on it.

| Value | Action |
|---|---|
| `none` | Default. Leave it running and unhealthy. |
| `restart` | Restart it: the first time at once, then after 30 s, 60 s, 120 s, 240 s and 300 s. After 5 restarts in 30 minutes, give up until it is healthy again or recreated. |
| `stop` | Stop it and leave it stopped. |

The healthcheck can come from the service or from the image. Setting
`restart` or `stop` with `healthcheck: { disable: true }` is an error.
Only running containers are acted on, so `gantry stop` and `down` are never
undone, and a project is skipped while a `gantry` command is working on it.

```yaml
services:
  app:
    image: example/app:1
    healthcheck: { test: [CMD, /app/check], interval: 30s, retries: 3 }
    health_on_failure: restart
```

### build

```yaml
services:
  app:
    build: ./app                 # or the long form:
    # build:
    #   context: ./app
    #   dockerfile: Containerfile
    #   dockerfile_inline: |
    #     FROM alpine:3
    #   args: { VERSION: "1.2" }
    #   target: runtime
    #   labels: { team: net }
    #   pull: true
    #   no_cache: true
    #   network: host
    #   cache_from: [registry.example.com/app:cache]
    #   tags: [registry.example.com/app:dev]
    #   extra_hosts: ["mirror:10.0.0.5"]
    #   shm_size: 256m
```

A service with `build:` and no `image:` is tagged `<project>-<service>`.
The image is built
when it is missing, on `gantry build`, on `up --build`, or every time with
`pull_policy: build`. Builds run through `docker build` (BuildKit);
kiri builds use the Docker CLI against kiri's socket with
the classic builder. Remote (git or URL) contexts are not supported.

Container names default to `<project>-<service>-1`.

## Runtimes

Gantry drives every runtime through the Docker Engine API on its socket.

| Runtime | Socket | How containers join the pod |
|---|---|---|
| `docker` | `/var/run/docker.sock` | Runtime `gantry` with network `none`, plus annotations that the `gantry-oci` shim turns into namespace paths before running runc. Run `gantry setup docker` once per host. |
| `kiri` | `/run/kiri/docker.sock` | Network, UTS and IPC modes `ns:<path>`. Needs the kiri patches in `contrib/kiri`. |

Services in one pod may use different runtimes.
