# Test Plan

| Field | Value |
|---|---|
| Product | Gantry |
| Version covered | 0.1.x, with the Docker Engine API engine layer |
| Status | Draft for review |
| Related | [Requirements](requirements.md), [Design](design.md) |

## 1. Approach

Gantry is tested at three levels:

| Tier | What | Where | Root |
|---|---|---|---|
| Unit | Pure logic: parsing, normalization, rendering, argument and request building, the shim's spec rewrite. | Any Linux machine, CI | No |
| Integration | The live suite in `tests/integration/run.sh`: real namespaces, links, nftables, DHCP and engines. | A Linux test host | Yes |
| Manual | Scenarios that need a real LAN or are hard to automate (reboot, live DHCP address change, mixed-runtime examples). | A Linux test host on a LAN segment | Yes |

Version 0.1.x is rapid development. Failing tests may be rewritten when
behavior changes on purpose. No tests are marked as regression tests yet.

## 2. Environments

| ID | Environment | Use |
|---|---|---|
| ENV-CI | GitHub Actions, `ubuntu-latest`, stable Rust | Format, clippy, unit tests, `cargo audit`, `cargo deny`. |
| ENV-LAB | Linux test host with Docker, Podman, patched kiri, nftables, dnsmasq, and the isolated DHCP bridge from `tests/integration/dhcp-lab.sh` | Full integration suite without touching a real network. |
| ENV-LAN | Same host, with a LAN segment that has free addresses and a gateway | macvlan and ipvlan against a real switch and router, examples, manual checks. |

Reference platform: Debian 13, kernel 6.12, Docker 29, Podman 5.4, kiri 0.2
with the `contrib/kiri` patch.

From a non-Linux workstation, `scripts/remote.sh` runs commands on a Linux
development host (set `GANTRY_DEV_HOST`).

## 3. Unit tests

Run with `cargo test --all-targets`.

| Test | File | Covers |
|---|---|---|
| `plain_and_braced`, `defaults`, `required`, `invalid` | `src/compose/interpolate.rs` | FR-CMP-02 |
| `env_file_parsing`, `merge_rules` | `src/compose/mod.rs` | FR-CMP-01, FR-CMP-02 |
| `implicit_pod_from_service_networks`, `network_mode_service_joins_pod` | `src/compose/normalize.rs` | FR-POD-06 |
| `errors_are_helpful` | `src/compose/normalize.rs` | FR-NET-07, FR-CMP-04, FR-CMP-05, FR-ADDR-01 |
| `profiles_filter_services` | `src/compose/normalize.rs` | FR-CMP-03 |
| `durations`, `sizes`, `cli_format` | `src/compose/units.rs` | FR-CMP-03 |
| `docker_spec` | `src/engine/spec.rs` | FR-RT-05 |
| `podman_and_kiri_spec` | `src/engine/spec.rs` | FR-RT-06 |
| `podman_images_are_qualified` | `src/engine/spec.rs` | FR-RT-10 |
| `built_images` | `src/engine/spec.rs` | FR-CMP-09 |
| `hash_is_stable_and_tracks_changes` | `src/engine/spec.rs` | FR-CLI-02, FR-ST-02 |
| `references`, `registries`, `auth_entries` | `src/engine/auth.rs` | FR-RT-13 |
| `renders_rules`, `dhcp_and_expose` | `src/firewall.rs` | FR-FW-01, FR-FW-04, FR-FW-05, FR-FW-06 |
| `deny_rule_with_unknown_peer_fails_closed` | `src/firewall.rs` | FR-FW-07 |
| `unknown_peer_addresses_are_skipped` | `src/firewall.rs` | FR-FW-05 |
| `finds_bundle_and_subcommand` | `src/oci.rs` | FR-RT-05 |
| `rewrites_namespaces` | `src/oci.rs` | FR-RT-05 |
| `untouched_without_annotations_and_errors_on_missing_ns` | `src/oci.rs` | FR-RT-05, NFR-SEC-02 |
| `mac_is_stable_and_local` | `src/pod/link.rs` | FR-NET-04 |
| `resolv_roundtrip`, `hosts_dedupes_names` | `src/pod/files.rs` | FR-DNS-01, FR-DNS-04 |
| `ack_parsing` | `src/pod/dhcp.rs` | FR-ADDR-03, FR-ADDR-06 |
| `fnv_known_values` | `src/util.rs` | FR-CLI-02 |

### 3.1 Unit tests changed by the engine move

The engine move replaced CLI argument lists with API create bodies. The CLI
tests `docker_args`, `podman_and_kiri_args` and `hash_changes_with_pod_id`
became `docker_spec`, `podman_and_kiri_spec` and
`hash_is_stable_and_tracks_changes`, which check the create body: for
Docker, `Runtime: gantry`, `NetworkMode: none`, the three annotations, the
reserved labels and read-only `/etc` mounts; for Podman, `ns:` for network,
UTS and IPC, and the same for kiri.

### 3.2 Unit tests to add

| Planned test | Covers |
|---|---|
| Socket selection and `GANTRY_*_HOST` overrides per runtime | FR-RT-02, FR-RT-03, FR-RT-04, NFR-OPS-04 |
| Podman fallback order: socket, then `systemctl start podman.socket`, then private service | FR-RT-04 |
| Shim rejects paths outside allowed directories and paths with `..` | NFR-SEC-02 |
| Reserved `io.gantry.` labels rejected | FR-CMP-06 |
| Validation of hostnames, DNS options and firewall comments | FR-FW-08, NFR-SEC-03 |
| Start order with `depends_on` cycles and missing services | FR-CLI-01 |
| Dependency levels computed from `depends_on` | NFR-PERF-06 |
| Compose `build:` keys map to the right `docker build` and `podman build` arguments | FR-CMP-09 |
| `GANTRY_NFT` ignored in release builds | NFR-SEC-10 |

## 4. Integration tests

### 4.1 `tests/integration/dhcp-lab.sh`

Builds an isolated DHCP lab: bridge `gtdhcp0` with no uplink, a namespace
`gtdhcpsrv` running dnsmasq on 10.99.0.1, pool 10.99.0.100 to 10.99.0.150,
two minute leases. Pods use the bridge as their macvlan parent, so the
suite never touches a real network.

```sh
sudo tests/integration/dhcp-lab.sh up
sudo tests/integration/dhcp-lab.sh down
```

dnsmasq logs to `/run/gtdhcp.log`, which the DHCP scenario reads to confirm
the release.

### 4.2 `tests/integration/run.sh`

Needs root, `gantry` on `PATH`, the engines under test, and three free
addresses on a parent interface. Scenarios are arguments; with none it runs
`docker podman dhcp`. Further scenarios: `kiri`, `dhcpmove`, `ipv6`.

```sh
sudo tests/integration/dhcp-lab.sh up
sudo GT_PARENT=gtdhcp0 GT_SUBNET=10.99.0.0/24 GT_GATEWAY=10.99.0.1 \
     GT_IP1=10.99.0.11 GT_IP2=10.99.0.12 GT_IP3=10.99.0.13 \
     tests/integration/run.sh docker podman kiri dhcp dhcpmove ipv6
```

The e2e scenario (`tests/integration/e2e/gantry.yaml`) runs once per
runtime: pod `web` (services `server` and `client`, hostname `webhost`,
input allowed from pod `other` on 8080), pod `data` (redis with a
healthcheck, input allowed from pod `web` on 6379) and pod `other` (`probe`,
no firewall).

| Check | Scenario | Covers |
|---|---|---|
| `up` succeeds | e2e | FR-CLI-01, FR-POD-01, FR-RT-01 |
| Second `up` changes nothing | e2e | FR-CLI-02 |
| Pod has its static address on `eth0` | e2e | FR-NET-01, FR-NET-03, FR-ADDR-02 |
| Shared hostname | e2e | FR-POD-03, FR-RT-06 |
| Shared UTS, IPC and network namespaces (same inode in two containers) | e2e | FR-POD-02, FR-POD-05, FR-RT-05, FR-RT-06 |
| Loopback between services in a pod | e2e | FR-POD-05 |
| Service name resolves to 127.0.0.1 | e2e | FR-DNS-02 |
| Other pod reachable by service name | e2e | FR-DNS-03 |
| Firewall blocks `other` to `data:6379` | e2e | FR-FW-01, FR-FW-05 |
| Firewall allows `other` to `web:8080` | e2e | FR-FW-05 |
| Firewall blocks `data` to `web:8080` | e2e | FR-FW-05 |
| Default route via gateway | e2e | FR-NET-05 |
| nft table loaded in pod | e2e | FR-FW-01 |
| Host has no `gantry` table | e2e | FR-FW-02, NFR-SEC-01 |
| `ps` shows redis healthy | e2e | FR-CLI-05, FR-CLI-08 |
| Docker restart keeps the pod (Docker only) | e2e | FR-ST-04 |
| `down` removes the netns | e2e | FR-CLI-04 |
| `down` removes state | e2e | FR-CLI-04, FR-ST-01 |
| `up` with DHCP | dhcp | FR-ADDR-03 |
| Leased address in pool | dhcp | FR-ADDR-03 |
| Default route from lease | dhcp | FR-ADDR-03 |
| resolv.conf from lease | dhcp | FR-DNS-04 |
| DHCP server reachable | dhcp | FR-ADDR-03 |
| Lease released on `down` | dhcp | FR-ADDR-04 |
| Agent stopped after `down` | dhcp | FR-ADDR-04, NFR-SEC-04 |
| First lease from the first pool | dhcpmove | FR-ADDR-03 |
| New lease after the server's pool moves (NAK, then discover) | dhcpmove | FR-ADDR-04 |
| Watcher pod's hosts file follows the new address | dhcpmove | FR-ADDR-05, FR-DNS-03 |
| Watcher firewall allows the new address and drops the old | dhcpmove | FR-ADDR-05, FR-FW-05 |
| Pod has its static IPv6 address | ipv6 | FR-ADDR-01, FR-ADDR-02 |
| Hosts file lists both address families | ipv6 | FR-DNS-03 |
| ping6 by pod name | ipv6 | FR-DNS-03 |
| Firewall allows `pod:b` and blocks pod c over IPv6 | ipv6 | FR-FW-05 |

`dhcpmove` restarts the lab's dnsmasq with a different pool
(`dhcp-lab.sh move`) under a running pod and takes about a minute, because
it waits for the client's T1 renewal.

### 4.3 ipvlan

The e2e scenario reads `GT_DRIVER` and `GT_MODE`, so the same checks run
over ipvlan:

```sh
sudo GT_DRIVER=ipvlan GT_MODE=l2 GT_PARENT=... GT_SUBNET=... GT_GATEWAY=... \
     GT_IP1=... GT_IP2=... GT_IP3=... \
     tests/integration/run.sh docker podman
```

Covers FR-NET-02 with every e2e check above. Run it on a parent that has no
macvlan links at the time (kernel rule).

### 4.4 Engine API checks to add

These checks target the API-based engine layer and are added to `run.sh`:

| Planned check | Covers |
|---|---|
| Suite passes with the `docker` and `podman` CLIs absent from `PATH` (project without `build:`) | FR-RT-01, FR-RT-14 |
| Podman scenario passes with `podman.socket` enabled | FR-RT-04, FR-RT-09 |
| Podman scenario passes with `podman.socket` stopped but enabled (Gantry starts it) | FR-RT-04 |
| Podman scenario passes with `podman.socket` masked (private service fallback); the socket is mode 0600 and the service exits after 5 idle minutes | FR-RT-04, NFR-SEC-06 |
| Pull of an image from a registry that needs credentials | FR-RT-13 |
| `exec` with a TTY follows a terminal resize | FR-RT-14 |
| `gantry build` and `up --build` build a service on Docker and on Podman; plain `up` builds only when the image is missing | FR-CMP-09, FR-CLI-11 |
| `build:` on a kiri service builds through the classic builder | FR-CMP-09 |
| A project with several independent services starts them concurrently (start times overlap in engine events) | NFR-PERF-06 |
| `up` against a stopped engine fails before any pod exists and names the socket | FR-RT-12 |
| `exec` exit code and `logs --tail` match the container | FR-CLI-05 |
| Changing a service's runtime removes the old container | FR-RT-11 |
| `setup podman` enables the socket and is idempotent | FR-RT-09 |
| `setup docker` is idempotent and keeps other keys in `daemon.json` | FR-RT-09 |

### 4.5 Verified by hand

Run on the test host on 2026-11-15 against the API engine; candidates for
automation in 4.4.

| Check | Result |
|---|---|
| Podman scenario with `podman.socket` stopped (Gantry started it) | Passed |
| `exec`: exit code passed through, piped stdin, TTY on Docker and Podman | Passed |
| `logs --tail` across Docker and Podman services | Passed |
| `build:` on Docker and Podman in one pod, with build args; `up` builds only missing images | Passed |
| `build:` on kiri (classic builder), then `run` of the built image | Passed |
| `run`: exit code, `--rm`, stdin, TTY, named one-off listed by `ps`, removed by `down` | Passed |
| `kill`, `rm`, `top`, `ls`, `version` | Passed |
| `setup docker` and `setup podman` on a host with the prototype's runtime entry removed | Passed |
| Benchmark, 8 services in 2 pods: `up` 0.45 to 0.7 s, no-change `up` 80 ms, `ps` 45 ms, `down` 0.35 to 0.6 s | Recorded in the README |
| MT-02 (`examples/mixed` on vlan4): Docker, Podman and kiri share one UTS and network namespace | Passed |
| `examples/basic` on vlan4 with Docker and with Podman: gateway reachable, HTTPS egress, health-gated start | Passed (reaching the pods from another LAN machine not yet done) |
| MT-03: reboot with `gantry@basic` enabled | Passed: pods back on vlan4 with firewalls about 15 s after boot |
| Private registry with basic auth, credentials from `docker login` | Passed; fails cleanly after logout |
| A same-named container from outside the project is refused, not replaced | Passed |

## 5. Manual tests

Run on ENV-LAN before a release. Record revision, host and result.

| ID | Procedure | Expected | Covers |
|---|---|---|---|
| MT-01 | `examples/basic`: `up -d`, reach both pods from another LAN machine, `down`. | Pods answer at their addresses; nothing left in `ip netns` or `/run/gantry`. | FR-NET-01, FR-NET-06, FR-CLI-04 |
| MT-02 | `examples/mixed`: one pod with Docker, Podman and kiri services. | All three share the pod address and loopback. | FR-RT-07 |
| MT-03 | Reboot with `gantry@<name>` enabled. | Project is up after boot; old containers replaced. | FR-ST-02, FR-ST-03 |
| MT-04 | Change a DHCP reservation so the pod gets a new address on renew. | Other pods' hosts files and firewall rules follow the new address. | FR-ADDR-05 |
| MT-05 | Edit a pod's address and run `up`. | Pod and its containers are recreated; other pods untouched. | FR-POD-07 |
| MT-06 | Edit only firewall rules and run `up`. | Ruleset updated with no container recreated. | FR-POD-07, FR-FW-01 |
| MT-07 | Kill `gantry` during `up` (after a pod is created), then run `up` again. | Second `up` completes; no duplicate links or namespaces. | NFR-REL-02 |
| MT-08 | `firewall show`, `firewall reload`, `pod ls`, `pod inspect`, `pod exec`. | Output matches loaded state. | FR-FW-09, FR-CLI-07 |
| MT-09 | Time `up` for a 10-service project with images present, twice. | Targets in NFR-PERF-01 and NFR-PERF-02 met. | NFR-PERF-01, NFR-PERF-02 |
| MT-10 | Measure DHCP agent RSS and acquisition time. | Targets in NFR-PERF-03 and NFR-PERF-04 met. | NFR-PERF-03, NFR-PERF-04 |

## 6. Static and supply chain checks

| Check | Command | Covers |
|---|---|---|
| Format | `cargo fmt --check` | Maintainability |
| Lint, warnings denied, deny-lint block for unwrap, expect and panic | `cargo clippy --all-targets -- -D warnings` | NFR-SEC-07 |
| Vulnerabilities | `cargo audit` | NFR-SEC-09 |
| Licenses and sources | `cargo deny check` | NFR-SEC-09 |

## 7. Traceability

Requirements not listed here are covered only by review and are the first
candidates for new tests.

| Requirement | Checks |
|---|---|
| FR-POD-01 | e2e up, `down` removes netns |
| FR-POD-02, FR-POD-03 | e2e shared hostname |
| FR-POD-04 | e2e (no infra container is created; `ps` lists only services) |
| FR-POD-05 | e2e loopback |
| FR-POD-06 | unit normalize tests |
| FR-POD-07 | MT-05, MT-06 |
| FR-POD-08 | Review; add a check that a non-root container can bind port 80 |
| FR-POD-09 | Review |
| FR-POD-10 | Review; MT-07 |
| FR-NET-01 | e2e (macvlan), MT-01 |
| FR-NET-02 | ipvlan e2e |
| FR-NET-03, FR-NET-05 | e2e address and route checks |
| FR-NET-04 | `mac_is_stable_and_local` |
| FR-NET-06 | MT-01 |
| FR-NET-07 | `errors_are_helpful` |
| FR-ADDR-01, FR-ADDR-02 | `errors_are_helpful`, e2e address check |
| FR-ADDR-03, FR-ADDR-04 | dhcp scenario, `ack_parsing` |
| FR-ADDR-05 | dhcpmove (MT-04 automated) |
| FR-ADDR-06 | `ack_parsing`, review |
| FR-DNS-01 to FR-DNS-04 | unit files tests, e2e name checks, dhcp resolv.conf |
| FR-FW-01 to FR-FW-07 | unit firewall tests, e2e firewall checks |
| FR-FW-08 | Planned unit test (3.2) |
| FR-FW-09 | MT-08 |
| FR-RT-01 to FR-RT-04, FR-RT-12 | Planned engine API checks (4.4), e2e on each runtime |
| FR-RT-05, FR-RT-06 | unit engine and shim tests, e2e per runtime |
| FR-RT-07 | MT-02 |
| FR-RT-08, FR-RT-10, FR-RT-11 | unit engine tests, planned check (4.4) |
| FR-RT-13 | auth unit tests, planned check (4.4) |
| FR-RT-14 | Planned checks (4.4) |
| FR-RT-09 | Planned checks (4.4) |
| FR-CMP-01 to FR-CMP-08 | unit compose tests, planned label test |
| FR-CMP-09, FR-CLI-11 | `built_images`, planned unit test (3.2), build checks (4.4) |
| FR-CLI-01 to FR-CLI-10 | e2e, MT-08, planned checks |
| FR-ST-01, FR-ST-02 | e2e `down` checks, `hash_is_stable_and_tracks_changes`, MT-03 |
| FR-ST-03 | MT-03 |
| FR-ST-04 | e2e Docker restart |
| NFR-SEC-01 | e2e host has no `gantry` table |
| NFR-SEC-02 | shim unit tests, planned path tests |
| NFR-SEC-06 | Planned fallback check (4.4) |
| NFR-SEC-07, NFR-SEC-09 | Static checks (section 6) |
| NFR-PERF-01 to NFR-PERF-04 | MT-09, MT-10 |
| NFR-PERF-06 | Planned unit test (3.2) and concurrency check (4.4) |
| NFR-SEC-10 | Planned unit test (3.2) |

## 8. Release gate

A release candidate passes when:

1. CI is green on the candidate revision (section 6 and unit tests).
2. `run.sh docker podman kiri dhcp` passes on ENV-LAB, and the ipvlan run
   passes on a parent without macvlan links.
3. The engine API checks in 4.4 pass.
4. MT-01 to MT-08 pass on ENV-LAN, or each failure has a recorded waiver.
5. Results record the revision, host, kernel and engine versions. A skipped
   or unrun check is not a pass.

## 9. Known environment effects

Some lab networks reset plain HTTP between segments or only allow DNS to
specific resolvers. These are properties of the network, not Gantry.
Before filing a failure as a Gantry bug, reproduce it with a plain
`ip netns` and a manually created macvlan link. Prefer the isolated DHCP lab
for automated runs.
