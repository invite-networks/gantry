# Software Requirements Specification

| Field | Value |
|---|---|
| Product | Gantry |
| Version covered | 0.1.x (rapid development) |
| Status | Draft for review |
| Related | [Design](design.md), [Test plan](test-plan.md), [File format](../compose-spec.md) |

## 1. Purpose

Gantry is a pod-first container orchestrator for a single Linux host. It
reads the Compose file format, builds each pod's namespaces itself, attaches
the pod directly to a LAN segment with macvlan or ipvlan, applies an
nftables firewall inside the pod, and then starts the pod's containers with
Docker (the primary engine) or kiri. It also restarts services whose
healthchecks keep failing, when they opt in.

This document states what Gantry must do. Each requirement has a stable ID
that the [test plan](test-plan.md) traces back to. How the requirements are
met is in the [design](design.md).

## 2. Scope

### 2.1 In scope

* Pods built from pinned network, UTS and IPC namespaces with no infra or
  pause container.
* LAN attachment through macvlan or ipvlan, with static addresses or a
  built-in DHCPv4 client.
* A per-pod nftables firewall defined in the project file.
* Docker as the primary container runtime and kiri as a second one, mixed
  freely within one pod, both driven through the Docker Engine HTTP API.
* Self-healing of services that opt in with `health_on_failure`.
* Image builds (`build:`) through the `docker` build tool.
* A Compose-compatible file format with documented extensions.
* A command line interface modeled on `docker compose`.

### 2.2 Out of scope

* Multi-host scheduling, clustering and service discovery across hosts.
* Bridge networks, NAT and host port publishing.
* Replicas and scaling (`deploy.replicas`, `scale`). Ignored with a warning.
* Rootless operation.
* Podman. Supported in the 0.2 and 0.3 test releases, removed in 0.1.0.
* DHCPv6. IPv6 addresses come from SLAAC or static configuration.
* Any network listener of its own. Gantry exposes no TCP or HTTP port.

## 3. Actors

| Actor | Description |
|---|---|
| Operator | A person with root on the host who writes project files and runs `gantry` commands. |
| Service consumer | A machine on the LAN that reaches a pod at its LAN address. |
| Container engine | Docker (`dockerd`) or kiri. Creates and runs containers on request. |
| LAN infrastructure | The switch, router and DHCP server on the segment the pods join. |
| Init system | systemd, which starts registered projects at boot through `gantry-boot.service` and runs the health watcher as `gantry-supervise.service`. |
| gantry-agent | The Gantry Server agent. When present it runs the health watcher itself, through the library, instead of `gantry-supervise.service`. |

## 4. Definitions

| Term | Meaning |
|---|---|
| Project | One project file (plus overrides) and everything created from it. |
| Pod | A set of namespaces created by Gantry that one or more containers join. |
| Service | One container definition. Each service runs in exactly one pod. |
| Network | One entry in a pod's `network:` list: a parent interface on the host, an address with prefix (or DHCP), and a gateway. |
| Runtime | The engine that runs a service: `docker` or `kiri`. |
| Health watcher | The long-running loop (`gantry supervise`, or gantry-agent) that acts on unhealthy containers. |
| Engine socket | The Unix socket where a runtime serves the Docker Engine API. |

## 5. Functional requirements

Priority: **M** must, **S** should, **C** could.

### 5.1 Pods

| ID | Requirement | Priority |
|---|---|---|
| FR-POD-01 | Gantry shall create each pod's network namespace itself and pin it at `/run/netns/gt-<project>-<pod>` so `ip netns` and `ip -n` can use it. | M |
| FR-POD-02 | Gantry shall create and pin UTS and IPC namespaces for a pod when `share` includes them, which is the default. The PID namespace is never shared. | M |
| FR-POD-03 | Gantry shall set the pod hostname and optional domain name inside the shared UTS namespace. | M |
| FR-POD-04 | Pods shall run without an infra or pause container. Containers join the pinned namespaces by path. | M |
| FR-POD-05 | All services in a pod shall share the network namespace and reach each other on `127.0.0.1`. | M |
| FR-POD-11 | A top-level `pod:` shall define the project's only pod, named after the project. `pod:` together with `pods:` shall be rejected. | S |
| FR-POD-12 | A project name shall run from one directory at a time: every command shall refuse a project whose name has pods running from another directory. `up` shall refuse a static address that a pod of another project on the host holds (static or leased). | M |
| FR-POD-06 | A service without `pod:` shall join the project's pod when there is exactly one, and be rejected otherwise. `network_mode: service:<x>` or `container:<x>` shall place it in the other service's pod. `network_mode: none` shall give it a loopback-only pod. Pod settings (networks, hostname, DNS, firewall and similar) on a service shall be rejected. | M |
| FR-POD-07 | Gantry shall recreate a pod, and the containers in it, when its shape changes (addresses, networks, hostname, shared namespaces, sysctls). Firewall and hosts changes shall be applied without a recreate. | M |
| FR-POD-08 | Gantry shall apply pod `net.*` sysctls to the pod namespace and set `net.ipv4.ip_unprivileged_port_start=0` and `net.ipv4.ping_group_range=0 2147483647` unless the pod overrides them. | M |
| FR-POD-09 | A pod with `ipv6: false` shall have IPv6 disabled in its namespace. | S |
| FR-POD-10 | A failed pod creation shall leave no namespace, link, agent or state directory behind. | M |

### 5.2 Networking

| ID | Requirement | Priority |
|---|---|---|
| FR-NET-01 | Networks shall be defined only under the pod, as a `network:` list. Each entry shall be attached with a macvlan link in `bridge` mode on its `parent` interface. A top-level `networks:` section shall be rejected with an error that points to the pod form. | M |
| FR-NET-02 | `driver: ipvlan` shall attach the entry with an ipvlan link in `l2` mode instead. Projects that put macvlan and ipvlan links on one parent shall be rejected. | M |
| FR-NET-03 | Links shall be created directly inside the pod namespace and named `eth0`, `eth1`, and so on, or the name given by `interface:`. | M |
| FR-NET-04 | macvlan links shall get a stable, locally administered MAC derived from the project, pod and interface names unless `mac_address` is set. | M |
| FR-NET-05 | The first entry shall carry the default route unless another sets `default_route: true`. The IPv4 gateway shall be `gateway:` or, when omitted, the first host of the address's subnet (10.10.10.1 for 10.10.10.10/24). A /31 or /32 default-route entry without `gateway:` shall be rejected. The IPv6 gateway shall be `gateway6:` or come from router advertisements. | M |
| FR-NET-06 | Gantry shall send gratuitous ARP whenever it assigns an IPv4 address. | M |
| FR-NET-07 | `driver: bridge` and any driver other than macvlan or ipvlan shall be rejected with an error that explains why. | M |
| FR-NET-08 | Links shall inherit the parent interface's MTU. | S |

### 5.3 Addressing

| ID | Requirement | Priority |
|---|---|---|
| FR-ADDR-01 | Every pod network entry shall have `address:`: an IPv4 address with prefix length, `dhcp`, or `{ from: <plugin>, ... }` (FR-ADDR-07), plus an optional static `address6:` with the first form. Gantry shall never pick LAN addresses on its own. | M |
| FR-ADDR-02 | An address without a prefix length, or that is its subnet's network, broadcast or gateway address, shall be rejected with an error that says so. | M |
| FR-ADDR-03 | With `address: dhcp` Gantry shall acquire an IPv4 lease with a DHCP client that runs inside the pod namespace, sends a client identifier, and applies the address, default route, DNS servers and domain from the lease. | M |
| FR-ADDR-04 | The DHCP client shall renew at T1, rebind at T2, drop the address at expiry and reacquire, and release the lease when the pod is removed. | M |
| FR-ADDR-05 | When a DHCP pod's address changes, Gantry shall refresh every pod's hosts file and firewall in the project. | S |
| FR-ADDR-06 | Lease values (address, mask, domain name) shall be validated before they are applied or written to `/etc/resolv.conf`. | M |
| FR-ADDR-07 | With `address: { from: <name>, ... }` Gantry shall ask the plugin that provides `<name>` for an IPv4 address and optional gateway only when it creates the pod, passing the other keys, the network entry and any previous address. A compose `gateway:` wins over the plugin's. An unusable reply, an address another pod on the host holds, or any plugin failure shall fail `up` before the pod exists. A repeated `up` with the same configuration shall keep the pod and its address without asking. | M |

### 5.4 Names and shared files

| ID | Requirement | Priority |
|---|---|---|
| FR-DNS-01 | Gantry shall write `/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname` once per pod and bind mount them read only into every container of the pod. | M |
| FR-DNS-02 | In a pod's hosts file, the pod hostname, pod name and its service names shall resolve to `127.0.0.1` (IPv4 only). | M |
| FR-DNS-03 | Other pods shall resolve to their first IPv4 and first IPv6 address under their hostname, pod name and service names. | M |
| FR-DNS-04 | DNS servers shall come from the pod's `dns:`, else the DHCP lease, else the host's upstream servers with loopback resolvers removed. | M |
| FR-DNS-05 | Hosts files shall be rewritten in place so existing bind mounts see changes. | M |

### 5.5 Firewall

| ID | Requirement | Priority |
|---|---|---|
| FR-FW-01 | Every pod with a network shall get one `inet gantry` nftables table inside its network namespace, replaced atomically on each apply. | M |
| FR-FW-02 | Gantry shall never add, change or remove rules in the host's network namespace. | M |
| FR-FW-03 | Input and output policies shall default to accept. Unless `defaults: false`, Gantry shall add an accept for ICMP and one for ICMPv6 before the pod's rules and a reject from the pod's own subnets (`subnet`) after its rules and ports, whether or not the pod has a `firewall:` section. | M |
| FR-FW-04 | Loopback, established and related traffic, IPv6 neighbor discovery, essential ICMP errors and DHCP replies (on DHCP pods) shall always be allowed. | M |
| FR-FW-05 | Rules shall match protocol, ports and port ranges, interface, and peers given as CIDR, host, `pod:`, `service:`, `subnet` (the pod's own subnets, static or leased) or `any`, with actions accept, drop or reject, optional rate limit, logging and comment. | M |
| FR-FW-06 | Service `ports:` shall open the container port to everyone. A host mapping such as `8080:80` opens the container port and warns. `expose:` shall open the port to the project's other pods only. | M |
| FR-FW-07 | A deny rule whose peer pod has no known address shall fail with an error rather than be skipped. | M |
| FR-FW-08 | Rule fields that reach the ruleset (comments, limits, interface names) shall be validated so they cannot inject nftables syntax. | M |
| FR-FW-09 | `firewall show` shall print every pod's loaded rules with hit counts, or one pod's when named, and with `--raw` the generated ruleset and what is loaded. Every rule and each chain's policy shall be counted. `firewall reload` shall re-render and apply every pod's firewall and hosts file. | M |

### 5.6 Runtimes and engines

| ID | Requirement | Priority |
|---|---|---|
| FR-RT-01 | Gantry shall drive every runtime through the Docker Engine HTTP API over a Unix socket. The `docker` command line tool shall not be required, except to build images for services with `build:` (FR-CMP-09). | M |
| FR-RT-02 | Docker shall be reached at `/var/run/docker.sock`. | M |
| FR-RT-03 | kiri shall be reached at `/run/kiri/docker.sock`. | M |
| FR-RT-04 | Removed in 0.1.0 (Podman socket). | - |
| FR-RT-05 | Docker containers shall join the pod through the `gantry-oci` runtime shim, using the `gantry` runtime, network mode `none`, and `io.gantry.netns`, `io.gantry.utsns` and `io.gantry.ipcns` annotations. | M |
| FR-RT-06 | kiri containers shall join the pod's network, UTS and IPC namespaces with modes `ns:<path>` (kiri with the patches in `contrib/kiri`). | M |
| FR-RT-07 | Services in one pod shall be allowed to use different runtimes. | M |
| FR-RT-08 | The runtime shall be selectable per project (`runtime:`), per pod, per service, and forced for all services with `--runtime`. | M |
| FR-RT-09 | `gantry setup docker` shall register the shim in the Docker daemon configuration and reload `dockerd`. | M |
| FR-RT-10 | Removed in 0.1.0 (Podman image names). | - |
| FR-RT-11 | When a service's runtime changes, its container on the previous runtime shall be removed. | M |
| FR-RT-12 | Gantry shall check that each engine needed by the command is reachable before changing anything, and report which socket failed. | M |
| FR-RT-13 | Image pulls shall use registry credentials from the Docker auth file and its credential helpers. | M |
| FR-RT-14 | `exec` (including interactive TTY with resize), `logs`, pulls, container lifecycle, volumes and health waits shall all use the engine API. | M |

### 5.7 Compose compatibility

| ID | Requirement | Priority |
|---|---|---|
| FR-CMP-01 | Gantry shall find `gantry.yaml`, `gantry.yml`, `compose.yaml`, `compose.yml`, `docker-compose.yaml` or `docker-compose.yml`, merge a matching override file, and merge any number of `-f` files left to right with the documented merge rules. | M |
| FR-CMP-02 | Gantry shall support Compose variable interpolation from the environment and `.env` (or `--env-file`), including all default, required and alternate forms, `$$`, YAML anchors and merge keys. | M |
| FR-CMP-03 | Gantry shall support the service keys listed in the [file format](../compose-spec.md), including volumes (bind, named, anonymous, tmpfs), healthchecks, `depends_on` conditions and profiles. | M |
| FR-CMP-04 | Unsupported keys (`logging`, `deploy`, `scale`, `links`, `external_links`, `platform`, unknown keys) shall be ignored with a warning. | M |
| FR-CMP-05 | Pod-level settings on a service that also has `pod:` shall be an error. | M |
| FR-CMP-06 | Labels under `io.gantry.` shall be reserved and rejected in project files. | M |
| FR-CMP-07 | Named volumes shall be created in the service's engine as `<project>_<name>` unless external. | M |
| FR-CMP-08 | The project name shall come from `name:`, `-p`, `COMPOSE_PROJECT_NAME`, then the directory name. | M |
| FR-CMP-10 | A service without `cpus` shall get 1 CPU and one without `mem_limit` 2 GiB. `0` shall mean no limit. A negative `cpus` shall be an error. | M |
| FR-CMP-11 | Before a file is parsed into a tree, it shall be refused past 200,000 values or 4 MiB of keys and scalars once aliases are expanded, without building the expansion. `${...}` shall nest at most 16 deep. Both shall be errors with a location, never a crash. | M |
| FR-CMP-12 | `<<` merge keys shall be applied until none remain, so keys inside a nested merge source are kept. A file still holding `<<` after 16 passes shall be an error. | M |
| FR-CMP-13 | `compose::check(text, options)` shall validate compose text with the same rules as loading, reading no file and no process environment. Variables come only from the caller's lookup, and a missing required variable is not an error. It shall return its issues (message, path where known, line and column for syntax errors and limits) together with the variables used without and with a default, the network parents (an entry needing an unresolved variable as written), and the `env_file` entries, the lists filled in even when there are issues. | M |
| FR-CMP-14 | `LoadOptions.isolated_env` shall take variables, `COMPOSE_PROFILES` and `COMPOSE_PROJECT_NAME` only from `LoadOptions.vars` and the env files, never the process environment, and refuse `~/` paths. `LoadOptions.vars` shall win over every other source. | M |
| FR-CMP-09 | `build:` shall be supported (context, Dockerfile, args, target and the other Compose build keys) by running `docker build` for Docker services, so BuildKit, `.dockerignore` and build secrets behave as with Compose. For kiri services, the image is built with `docker build` against the kiri socket using the classic builder. | M |

### 5.8 Command line

| ID | Requirement | Priority |
|---|---|---|
| FR-CLI-01 | `up` shall pull or build missing images, create volumes, create pods, apply hosts files and firewalls, then create and start services in dependency order. Options: `-d`, `--wait`, `--force-recreate`, `--pull`, `--build`, wait timeout, service list. | M |
| FR-CLI-02 | A second `up` with no changes shall not recreate any container or pod. Changes are detected through a configuration hash label. | M |
| FR-CLI-03 | `up` without `-d` shall follow logs from the moment it started, so output services print while they start is shown and kept containers do not replay their history, and stop services on interrupt while leaving pods up. | M |
| FR-CLI-04 | `down` shall remove containers in reverse dependency order, then pods (namespaces, links, DHCP leases, state). `-v` also removes named volumes. `-t` sets the stop timeout. | M |
| FR-CLI-05 | `ps`, `logs [-f] [--tail N]`, `exec [-T] [-u] [-w] [-e]`, `start`, `stop`, `restart` and `pull` shall behave as their `docker compose` counterparts. | M |
| FR-CLI-06 | `config` shall validate and print the normalized project, with `--services`, `--pods` and `--json`. | M |
| FR-CLI-07 | `pod ls`, `pod inspect` and `pod exec` shall list pods, show pod state as JSON (including leases), and run a host command inside a pod's network namespace. | M |
| FR-CLI-08 | `depends_on` shall honor `service_started`, `service_healthy` and `service_completed_successfully`, and fail clearly when a condition can never be met. | M |
| FR-CLI-09 | Commands that change a project shall hold a per-project lock so two runs cannot interleave. | M |
| FR-CLI-10 | Global flags: `-f`, `-p`, `--project-directory`, `--env-file`, `--profile`, `--runtime`, `--log-level`. | M |
| FR-CLI-11 | `build [svc...]` shall build the images of services that have `build:`. `up --build` shall build before starting. | M |
| FR-CLI-12 | `run [--rm] [-T] [--no-deps] [-e] [-u] [-w] [--entrypoint] svc [cmd...]` shall start a one-off container of the service in its pod, attached, bring up its dependencies unless `--no-deps`, and exit with the container's exit code. One-off containers shall be listed by `ps` and removed by `down`. | M |
| FR-CLI-13 | `kill [-s SIG]`, `rm [-s]` and `top` shall signal services, remove stopped service containers, and list service processes. | S |
| FR-CLI-14 | `ls` shall list projects with pods on the host; `version` shall report Gantry's version and each engine's version and API version. | S |
| FR-CLI-15 | `completion <shell>` shall print shell completions; the package shall install completions for bash, zsh and fish and a man page. | S |
| FR-CLI-17 | `data move` shall find every bind or volume mount whose running source differs from the project file, copy it to the new host path with rsync (`-aHAX --numeric-ids --sparse --delete`) in live passes, stop only the services that use it, run a final pass and a checksum verification, and recreate the services. It shall never delete the old data, refuse a destination that holds other data unless `--force`, and restart the services on the old data if the cutover fails. | S |
| FR-CLI-18 | `up` shall refuse to recreate a service onto a new data path that is empty, or holds an unfinished move, while the old path has data. | M |
| FR-CLI-19 | When the installed `/usr/bin/gantry` runs as a non-root member of the `gantry` group, it shall run itself again through `sudo -n` with the same arguments and working directory, except for `completion` and `internal` helpers. A non-member without root shall be told to run it as root or join the group. | M |
| FR-CLI-20 | `plugin ls` shall list installed plugins, their hooks and modes, the address source they provide, and whether each is enabled on the host and for the project in the current directory. | S |
| FR-CLI-21 | `stats [--no-stream] [--json] [--interval N] [svc...]` shall show, per running container, CPU use as a percent of one CPU with its limit and the share of scheduler periods throttled, memory without reclaimable cache against its limit, process count and block I/O rates, and per pod the sum of those plus the network rates of the pod's interfaces (loopback excluded), counted once. Figures come from two readings `--interval` seconds apart. A container whose engine reports no figures shall show `-` and a note. | S |
| FR-CLI-22 | `logs -f`, and the logs `up` follows, shall keep following a service whose container stops, restarts or is recreated, resuming with the new run. | M |
| FR-CLI-23 | `pull`, and the pulls `up` makes, shall show one line per image: download progress by bytes and layers on a terminal, and a final line saying whether the image was updated (old and new ID), was already up to date, or was pulled new. Without a terminal only the final lines are printed. | S |
| FR-CLI-24 | `-f FILE` shall be accepted before the command only, so commands keep their own `-f` (`logs -f`, `rm -f`). | M |
| FR-CLI-16 | Services in the same dependency level shall be created and started in parallel; `down` and `stop` shall act on reverse levels in parallel; pulls shall run in parallel. | M |

### 5.9 State and lifecycle

| ID | Requirement | Priority |
|---|---|---|
| FR-ST-01 | Runtime state shall live under `/run/gantry` and `/run/netns` and in container labels, and nowhere else. | M |
| FR-ST-02 | After a reboot, `up` shall recreate pods and replace containers that belonged to the previous pod instances. | M |
| FR-ST-03 | `gantry-boot.service` shall start every registered project at boot, after network and remote file systems, and stop them at shutdown. `gantry up -d` shall register an installed project and `gantry down` remove it; `stop`, `up` without `-d` and projects outside the project directories shall not change registration. `--installed NAME` shall find the project directory `NAME` in exactly one of the parent directories listed in `/etc/gantry/project-dirs` (default `/etc/gantry`, packaged with `/etc/gantry` and `/mnt/*`), never following symlinks. | S |
| FR-ST-06 | Pod `priority` (default 100, lower first) shall order pods across all registered projects at boot. Equal priorities start together; each level waits for its services to be running or healthy, up to a timeout, and a failing project does not stop the others. Shutdown goes in reverse. | M |
| FR-ST-07 | Service `priority` (default 100, lower first) shall order services inside their pod as implied `depends_on` (healthy when the lower service has a healthcheck, else started). Equal priorities start together. A lower-priority service depending on a higher one in the same pod shall be rejected. | M |
| FR-ST-04 | A container restarted by its engine (restart policy or manual restart) shall rejoin its pod. | M |
| FR-ST-05 | A service without `restart:` shall get `unless-stopped`. `restart: "no"` shall opt out. One-off `run` containers never get a restart policy. | M |
| FR-ST-08 | A registry entry shall hold the options a project is loaded with at boot. `boot::register_with` with `isolated_env` shall make boot load the project as `LoadOptions::isolated_env` does. A plain `register` (`gantry up`) shall add a default entry and leave an existing one, and its options, unchanged. An entry boot cannot read shall stop that project with an error rather than start it with other options. `boot ls` shall show each project's mode. | M |

### 5.10 Self-healing

| ID | Requirement | Priority |
|---|---|---|
| FR-HEAL-01 | A service shall accept `health_on_failure` with `none` (default), `restart` or `stop`. Any other value, or `restart` or `stop` with the healthcheck disabled, shall be a configuration error. | M |
| FR-HEAL-02 | The value shall be recorded on the container as the label `io.gantry.health-on-failure` (absent for `none`), so the watcher needs no project files and changing the value recreates the container. | M |
| FR-HEAL-03 | The health watcher shall act only on running containers that carry the label and that their engine reports `unhealthy`, on Docker and kiri alike. It shall never start a stopped container. | M |
| FR-HEAL-04 | Before acting, the watcher shall take the project's command lock without waiting, skip the project for that poll if a `gantry` command holds it, and inspect the container again under the lock. | M |
| FR-HEAL-05 | `restart` shall restart through the engine's restart endpoint: the first restart at once, later ones after a wait that doubles from 30 seconds to at most 5 minutes. After 5 restarts within 30 minutes it shall give up on that container until it is healthy again or recreated. | M |
| FR-HEAL-06 | `stop` shall stop the container through the engine and leave it stopped. | M |
| FR-HEAL-07 | Only one watcher shall run per host, enforced by an exclusive lock on `/run/gantry/heal.lock`. | M |
| FR-HEAL-08 | The watcher shall be a library function that takes a cancellation token, a per-project pause control and a channel for its actions, and returns once any action in progress has finished. Pausing a project shall return only when no action on it is in progress, and no action shall start on a paused project. Sending an action shall never block the watcher. | M |
| FR-HEAL-09 | Each action shall be reported with project, pod, service, container name and ID, engine, kind (`restarted`, `stopped`, `backing_off`, `gave_up`, `failed`), attempt, cap, the next backoff delay, the trigger, any error, and a UTC timestamp, and shall be serializable. | M |
| FR-HEAL-10 | `gantry supervise` shall run the watcher for Docker and kiri until SIGTERM or SIGINT. The package shall install `gantry-supervise.service`, enable it on first install, and keep it disabled across upgrades once an operator or gantry-agent disables it. | M |
| FR-HEAL-11 | `gantry version` shall list the features of the build, including `health_on_failure` and `supervise`, and the library shall export the same list. | S |

### 5.11 Plugins

| ID | Requirement | Priority |
|---|---|---|
| FR-PLUG-01 | Gantry shall find plugins as `<dir>/<name>/plugin.toml` under `/usr/local/lib/gantry/plugins` then `/usr/lib/gantry/plugins`, the first manifest with a name winning. A manifest with another `api`, a missing `exec`, an unknown hook, a bad name, or a file not owned by root (or the running user) or writable by group or others shall be skipped with a warning. | M |
| FR-PLUG-02 | A plugin shall be started only when a command reaches a hook it subscribed to, at most once per command, in its own session, with its directory as working directory and only `PATH`, `LANG`, `LC_ALL`, `GANTRY_PLUGIN_NAME` and `GANTRY_PLUGIN_CONFIG` in its environment. Its stderr shall be appended to `/var/log/gantry/plugins/<name>.log`. | M |
| FR-PLUG-03 | Gantry shall speak JSON lines on the plugin's stdin and stdout: `hello` first (API version, gantry version, host, project, lock file), then `event` and `allocate` messages. Sync events and `allocate` carry an `id` and a `deadline_ms`; answers carry the `id` and `ok`, with `result` or `error`. | M |
| FR-PLUG-04 | Hooks shall be `pre_up`, `post_up`, `pre_down`, `post_down`, `post_start`, `post_stop`, `post_restart`, `heal_restart` and `boot`. `restart` shall send one `post_restart` and no `post_stop` or `post_start`. | M |
| FR-PLUG-05 | Each hook shall be `sync` (wait for the answer up to its timeout) or `async` (return once the message is written), as the manifest says unless `/etc/gantry/plugins.toml` overrides it on the host. Hooks shall fail open: a missing, crashing, slow or failing plugin is a warning and never changes the command's result. | M |
| FR-PLUG-06 | Event data shall describe each pod (name, netns, hostname, state, interfaces with parent, driver, addresses, gateway and MAC) and its containers (service, name, ID, engine, image, state, health, restart, command, limits, labels, mounts), plus the project's `plugins.<name>` settings. Environment variables and credentials shall never be sent. `post_down` shall report every pod and container as `down`. | M |
| FR-PLUG-07 | Every installed plugin shall run for every project unless the host config disables it or the project sets `plugins.<name>.enabled: false`. A `plugins:` block for a plugin that is not installed shall be ignored. | M |
| FR-PLUG-08 | A plugin that exits early shall never stop gantry: writes to it shall not raise SIGPIPE in gantry, and pending requests shall fail as soon as its stdout closes. | M |

### 5.12 Pod certificates

| ID | Requirement | Priority |
|---|---|---|
| FR-TLS-01 | `pod.tls` shall require fully qualified `names` and accept `key_type` (ec256, rsa2048, rsa4096), `optional` and `issuer`. A service `tls` shall require `on_renew` (`{ signal }`, `{ exec }`, `restart`, `none`), and accept `dir` (absolute, default `/etc/tls`), `files` (name to content and format, default cert.pem, chain.pem, fullchain.pem, key.pem, ca.pem) and `owner`. A service with `tls` in a pod without it shall be an error. `tls` shall not be part of the pod's configuration hash. | M |
| FR-TLS-02 | `up` shall give each pod with `tls` a certificate from the issuer plugin (`[certificate]` in its manifest; `issue`, `renew`, `status` requests) before its containers start, reusing a current one for the same names and key type. A failure shall stop `up` unless the pod's tls is `optional`. | M |
| FR-TLS-03 | Each service's files shall be written in its layout and ownership (key material 0400, the rest 0444) under the pod's directory in `/run/gantry`, replaced by rename, and that directory mounted read only at `dir`. Files no longer named shall be removed. | M |
| FR-TLS-04 | The certificate watcher (`gantry-tls.service` running `gantry internal tls-watch`, enabled by the package and separate from the health watcher; `tls::watch` for library callers; one per host by an exclusive lock on `/run/gantry/tls.lock`) shall mint a new certificate when one is past two thirds of its lifetime, when `gantry internal tls-refresh` has marked the issuer's credential changed since it was issued, or when its names or key type changed; renew it when minting fails; then rewrite every service's files and run each service's `on_renew`. It shall skip a project a `gantry` command holds. | M |
| FR-TLS-05 | Certificate problems (none issued, mint or renew failed with the issuer's text, within 4 hours of its end or expired, and the issuer's own `status` problems) shall be logged as errors, printed on stderr, and sent as a `tls_problem` plugin event, repeated at most every 30 minutes. `ps` shall show each pod's certificate and warn on stderr when one needs attention. | M |

## 6. Non-functional requirements

### 6.1 Platform

| ID | Requirement |
|---|---|
| NFR-PLAT-01 | Gantry shall run on Linux only, as root. Rootless operation is not supported. |
| NFR-PLAT-02 | Reference platform: Debian 13, kernel 6.12 or later, with nftables. Other current distributions with the same kernel features are expected to work. |
| NFR-PLAT-03 | Required host tools: `nft` for pods with a firewall. At least one engine: Docker Engine with API 1.43 or later (annotations), or kiri 0.2 with the `ns:<path>` patch. The `docker` CLI only for projects that use `build:`. `runc` (or another OCI runtime named by `GANTRY_OCI_RUNTIME`) for Docker. |
| NFR-PLAT-05 | The library without its default `runtime` feature shall be the compose loader and `compose::check` alone, and shall build and pass its tests on macOS as well as Linux, for callers that validate project files off the host. | S |
| NFR-PLAT-04 | Gantry shall be distributed as two static-friendly binaries, `gantry` and `gantry-oci`, built with Rust 2024 edition. |

### 6.2 Security

| ID | Requirement |
|---|---|
| NFR-SEC-01 | The host firewall and host network configuration shall not be changed, except for creating macvlan or ipvlan child links on configured parents and pinning namespaces. |
| NFR-SEC-02 | The shim shall accept only namespace paths under `/run/netns/`, `/var/run/netns/` and `/run/gantry/`, reject `..`, and shall not let an annotation choose the runtime binary. |
| NFR-SEC-03 | All project input that reaches the kernel, nftables, hosts files or resolv.conf shall be validated (hostnames, DNS search and options, firewall fields, interface names). |
| NFR-SEC-04 | Signals shall only be sent to a DHCP agent after its PID file is verified against `/proc/<pid>/cmdline`. |
| NFR-SEC-05 | Namespace files and state files shall be created race-free and written atomically. |
| NFR-SEC-06 | Removed in 0.1.0 (private Podman API service). |
| NFR-SEC-07 | The library shall not panic in production paths (no `unwrap`, `expect` or `panic` outside tests). |
| NFR-SEC-08 | Random identifiers shall come from the operating system's random source. |
| NFR-SEC-09 | Dependencies shall pass `cargo audit` and `cargo deny` in CI. |
| NFR-SEC-10 | The `GANTRY_NFT` binary override shall be honored only in debug builds. Release builds always run `nft` from `PATH`. |
| NFR-SEC-12 | `/etc/sudoers.d/gantry` shall grant members of the `gantry` group `/usr/bin/gantry` as root without a password and with their environment, The remote control account shall be `gantry-remote` in its own group, never a member of `gantry`; upgrading from the pre-0.2.0 `gantry` account shall rename it in place, keeping its uid. Documentation shall state that membership is equivalent to root. | M |
| NFR-SEC-13 | The `GANTRY_PLUGIN_DIR` search override shall be honored only in debug builds. | M |
| NFR-SEC-11 | A helper shall run the gantry binary that is running, when that binary is named `gantry`, and otherwise `/usr/bin/gantry`. Neither configuration nor the environment shall choose the helper binary. |

### 6.3 Performance

Targets, to be measured by the test plan. They exclude image pulls and time
spent waiting on DHCP servers or healthchecks.

| ID | Requirement |
|---|---|
| NFR-PERF-01 | Creating a pod with static addressing shall take under 1 second. |
| NFR-PERF-02 | A no-change `up` on a project of 10 services shall finish in under 3 seconds. |
| NFR-PERF-03 | DHCP acquisition shall complete within 10 seconds on a responsive server. |
| NFR-PERF-04 | A DHCP agent shall use under 10 MB resident memory. |
| NFR-PERF-05 | Engine calls shall reuse one API connection per engine per command rather than spawning processes. |
| NFR-PERF-06 | Engine operations shall run in parallel where order allows: images pull concurrently, services at the same dependency level are created and started concurrently, and `down` stops each level concurrently. |

### 6.4 Reliability

| ID | Requirement |
|---|---|
| NFR-REL-01 | `down` and pod teardown shall continue past individual failures and report the first error. |
| NFR-REL-02 | A stale namespace left by a crash shall be replaced on the next `up`. |
| NFR-REL-03 | The DHCP agent shall survive transient errors and keep retrying. |
| NFR-REL-04 | Engine API calls shall have explicit timeouts, except log following and attached exec sessions. |
| NFR-REL-05 | A DHCP agent shall not depend on the process or service that started it: under systemd it runs in its own transient scope, so stopping or restarting the caller (the CLI, `gantry-boot.service`, or a library caller such as gantry-agent) leaves it running. |

### 6.5 Operability

| ID | Requirement |
|---|---|
| NFR-OPS-01 | Error messages shall name the layer (config, engine, network, firewall, dhcp, state) and the object involved. |
| NFR-OPS-02 | Log level shall be set with `--log-level` or `GANTRY_LOG`. Logs go to stderr. DHCP agents log to a file in the pod directory. |
| NFR-OPS-03 | Pod namespaces shall be usable with standard tools (`ip netns`, `nft`, `tcpdump` through `pod exec`). |
| NFR-OPS-04 | Engine socket paths shall be overridable through environment variables for testing. |

## 7. Constraints and assumptions

* A parent interface carries macvlan or ipvlan links, not both (kernel rule).
* The host cannot reach its own macvlan pods through the parent interface.
* ipvlan links share the parent's MAC, so DHCP servers must key leases on
  the client identifier.
* Pods depend on the LAN for DNS and routing.
* Docker cannot join a namespace by path through its API, hence the shim.
* Stock kiri 0.2 does not support `ns:<path>`; the patch in `contrib/kiri` is
  required.
* The design assumptions apply: opt-in firewall, no address allocation,
  unshared PID namespace.

## 8. Acceptance criteria

A release is acceptable when:

1. Every **M** requirement maps to at least one passing check in the
   [test plan](test-plan.md), or has a recorded waiver.
2. Unit tests, format, clippy with warnings denied, `cargo audit` and
   `cargo deny` pass in CI.
3. The live integration suite passes on the reference platform for Docker,
   kiri, DHCP, ipvlan, self-healing and plugins, using the API-based
   engine layer.
4. The runbooks have been executed on a test host by someone other than
   their author.
