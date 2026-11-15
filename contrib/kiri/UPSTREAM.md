# Upstream submission to libracoder/kiri (DRAFT, not submitted)

Kiri's CONTRIBUTING.md asks for an agreed issue before any pull request
larger than a small fix, so this goes in two steps:

1. Open the feature request (Part 1). Wait for the maintainers to agree on
   the approach.
2. Then open the pull request (Part 2), linking the issue.

Everything below is a draft for review. Nothing has been pushed, opened or
signed.

## Compliance checklist

| Rule (source) | Status |
|---|---|
| Open an issue first for anything larger than a small fix, and wait for agreement (CONTRIBUTING, feature form) | Part 1 drafted. Not opened. |
| One change per pull request (CONTRIBUTING, PR template) | Network, UTS and IPC `ns:` modes combined into one commit, `414a5bc` on branch `pr-ns-path` on the test host. The earlier two-commit branch is kept. |
| Small and fast: no new dependency, background process or startup time (CONTRIBUTING) | No new dependencies, no new process. The default path (no `ns:` mode) runs the same code as before. |
| Speed check: main versus the change, `scripts/perf.py` limits (CONTRIBUTING) | Run on GitHub with Kiri's own `perf-run.sh`: **within the limits** (numbers in Part 2). It runs again automatically on the pull request. |
| Every feature ships on macOS, Windows and Linux together, built and tested on each; no OS gate (CONTRIBUTING) | No OS gate. **Linux:** tested end to end on a host and on a clean GitHub runner. **macOS (Apple silicon):** built with `cargo xtask` and tested end to end on a real Mac, paths inside Kiri's VM. **Windows:** the engine there is the same static Linux `kiri`/`kiri-init` in WSL 2, built and tested as that static build on GitHub; `kiri-win.exe` does not depend on the changed crates and builds and passes its tests on `windows-latest`. **Open:** an end-to-end run on Windows (GitHub's Windows runners cannot run WSL 2), so the issue asks the maintainers what they want there. |
| `python3 scripts/ui-test.py` passes (CONTRIBUTING) | The change has no window or UI. It runs in Kiri's CI on every PR. **Not run here.** |
| `cargo fmt`, `cargo clippy` and the tests pass (CONTRIBUTING, PR template) | Lines the change adds or edits are rustfmt-clean (stable rustfmt 1.11). Upstream `main` itself is not: `cargo fmt --check` reports hundreds of diffs in untouched files, so the change does not reformat them (one change per PR). Clippy reports no new warnings. Unit tests: 6 new; run as root on GitHub, every test passes on both sides (the 6 failures on the lab host came from running unprivileged). `tests/e2e.sh`: identical results on `main` and the change. See Part 2. |
| Tests run before submitting (CONTRIBUTING: `cargo test`, `tests/e2e.sh`) | Done on Linux (host and GitHub) and macOS (Apple silicon), `main` and the final commit `414a5bc` compared each time. No new failures anywhere. |
| `scripts/ci/notices.sh` after dependency changes (CONTRIBUTING) | Not applicable: no dependency changes. |
| Sign your commits (CONTRIBUTING) | Done. SSH-signed commit `02cdd1b` (same content as `414a5bc`) on `pr-ns-path` in `~/Code/kiri-pr`; the key is registered on GitHub as a signing key. |
| Contributor License Agreement with Canestack Labs via CLA Assistant (CONTRIBUTING, CLA.md) | **Needs you.** Section 3 asks you to confirm you may grant the licence (employer rights) and that the contribution is your original work. See the AI note below. |
| Apache-2.0 licence for contributions (CONTRIBUTING) | Accepted by submitting. |
| At most 3 open pull requests (CONTRIBUTING, PR template) | **Check** your open PRs on the repo before opening. |
| Disclose AI tools used (PR template) | Final wording in Part 2. |
| How you found it (PR template) | "I use Kiri and ran into this myself". |
| Tested on (PR template) | Linux (Debian 13; Ubuntu 24.04 on GitHub), macOS 27 on Apple silicon, Windows (client build and tests only). |
| Code of Conduct: professional, respectful, no one's private information | Drafts name no people and contain no addresses, host names, internal IPs or emails. Commit author is the GitHub noreply address. |
| SECURITY.md: vulnerabilities go through private reporting, not public issues | Nothing here is a vulnerability in released Kiri. The FIFO hang was in this unreleased patch and is fixed. The issue discusses the feature's security properties openly, which is normal design discussion. If a maintainer considers joining arbitrary namespaces a security concern, move that discussion to private reporting. |

### Decisions

1. **Account:** `aasay`. The pull request comes from a public fork of
   `libracoder/kiri` under that account.
2. **Gantry is not named.** The drafts describe the use case generically.
3. **AI disclosure:** included, final wording in Part 2.
4. **Windows end to end:** not run; the issue asks the maintainers whether
   they need it before the pull request.

Test runs: private repo `invite-networks/kiri-pr-testing` (branches
`upstream-main`, `pr-ns-path`, and `ci` with the workflow), plus the
Apple silicon runs on the local Mac.

---

## Part 1: Feature request (issue)

Form: *Feature request*.

**Title:** Join existing namespaces by path: `ns:<path>` for NetworkMode, UTSMode and IpcMode

**What you're trying to do**

I run several containers as one pod: they share a network namespace (one
address on the LAN, talking to each other over loopback), a hostname and
IPC. The namespaces are created and pinned outside the container engine,
the way `ip netns add` does, and each container joins them by path. There
is no infra or pause container.

Podman supports this with `--network ns:<path>`, `--uts ns:<path>` and
`--ipc ns:<path>`, and Docker can do it through a wrapper OCI runtime. With Kiri,
`--network ns:/run/netns/pod` fails with `network ns:/run/netns/pod not
found`, and `UTSMode` and `IpcMode` other than the defaults are ignored, so
Kiri containers can't be pod members alongside Podman or Docker ones.

**What you'd like Kiri to do**

Accept Podman's `ns:<absolute path>` form for `HostConfig.NetworkMode`,
`UTSMode` and `IpcMode` and join those namespaces as they are:

- Kiri creates, changes and removes nothing in a joined namespace. With
  `ns:` networking there is no bridge, veth, address, published port or
  extra network, and options that would need Kiri to change the namespace
  (`-p`, `--mac-address`, `--link`) are refused, as Podman does.
- In a joined UTS namespace the hostname is the namespace's: Kiri sets none,
  and `/etc/hostname`, `$HOSTNAME` and `/etc/hosts` show it. `Hostname` and
  `Domainname` from the client are ignored rather than refused, so Docker
  clients that always send them keep working (Podman refuses them).
- `docker inspect` shows the modes, and `docker exec`, restart and restart
  policies land in the same namespaces.
- Other modes behave as before.

I have a working implementation (about 400 lines with tests, mostly in the
agent's container setup and the daemon's create path, no new dependencies),
tested on Linux and on macOS (Apple silicon). Before opening a pull request
I'd like to agree on two things:

1. **macOS and Windows.** There, containers run in Kiri's Linux VM, so the
   path names a namespace inside the VM, the same as Podman machine or
   Docker Desktop. The code has no OS gate. On macOS I tested it with
   another container's `/proc/<pid>/ns/*` inside the VM, and it works as on
   Linux. On Windows the engine is the same static Linux build in WSL 2, which
   I built and tested that way, but I have not run it end to end on a
   Windows machine. Is that enough under "every feature ships on all three
   systems", or would you like a Windows run first?
2. **Security.** A client with access to the daemon socket could name any
   namespace file, for example `/proc/1/ns/net` (the same as
   `--network host`) or `/proc/1/ns/ipc` (like `--ipc host`). Kiri already
   allows `--privileged` and `--network host` to such a client, so this adds
   no new capability. The agent only accepts an absolute path to a regular
   file that `NS_GET_NSTYPE` reports as the right namespace type, opened
   non-blocking. Is that the bar you want, or would you like `ns:` modes
   limited further (for example to `/run/netns` and `/var/run/netns`)?

**Would you like to build it?** Yes, once we've agreed on the approach.

---

## Part 2: Pull request (after the issue is agreed)

**Title:** Join existing namespaces by path: ns:<path> for network, UTS and IPC

### What this changes and why

Kiri containers can now join namespaces that were created and pinned
outside Kiri, using Podman's `ns:<absolute path>` form for
`HostConfig.NetworkMode`, `UTSMode` and `IpcMode`. Several containers given
the same paths share one network, hostname and IPC namespace, the way
Podman pod members do, without an infra container.

- The namespaces are used as they are. Kiri makes and removes nothing in
  them. With `ns:` networking there is no bridge, veth, address, published
  port or extra network; port publishing, a MAC address and links are
  refused at create.
- In a joined UTS namespace the hostname is the namespace's. The child sets
  no hostname or domain name, and `$HOSTNAME`, `/etc/hostname` and
  `/etc/hosts` show the namespace's hostname. `Hostname` and `Domainname`
  from the client are ignored, not refused, so Docker clients keep working.
- The IPC namespace is joined before the mounts, so `/dev/mqueue` belongs to
  it.
- The daemon checks each path is absolute and passes it to the agent as a
  new net option (`netns_path`, `utsns_path`, `ipcns_path`). The agent
  refuses anything that is not a regular file before opening it (a FIFO
  would block it, a device would run its driver), opens it non-blocking,
  checks its type with `NS_GET_NSTYPE`, and joins it in the child instead of
  creating a namespace.
- `docker inspect` shows the modes. `docker exec` and restart land in the
  same namespaces. Other modes behave as before.

Fixes #<issue number>

### How you tested it

`main` (193dfd7) and this change, side by side, in three places: a Linux
host (Debian 13, kernel 6.12, x86_64), a clean GitHub runner (Ubuntu 24.04,
the static x86-64 engine, which is also the engine Windows runs in WSL 2),
and a Mac (macOS 27, Apple silicon, built with `cargo xtask kernel`,
`disk`, `guest` and `host`).

**Unit tests** (`cargo test -p kiri -p kiri-init -p kiri-proto`, as root on
the GitHub runner):

```text
                 main                         this change
kiri             233 passed, 4 ignored        237 passed, 4 ignored
kiri-init         64 passed                    66 passed
kiri-proto         5 passed                     5 passed
```

**Joining namespaces by path.** On Linux, the namespaces are pinned with
the usual tools:

```sh
ip netns add nsdemo && ip -n nsdemo link set lo up
mkdir -p /run/nsdemo && touch /run/nsdemo/uts /run/nsdemo/ipc
unshare --uts=/run/nsdemo/uts --ipc=/run/nsdemo/ipc hostname pod-host
mkfifo /run/nsdemo/fifo
```

Two containers are then created through the API with `"Hostname": "ignored"`,
`"NetworkMode": "ns:/run/netns/nsdemo"`, `"UTSMode": "ns:/run/nsdemo/uts"`
and `"IpcMode": "ns:/run/nsdemo/ipc"`, and print their hostname and
`readlink /proc/self/ns/{net,uts,ipc}`. A third points its network at the
FIFO.

Before (`main`):

```text
$ create nsdemo-a with NetworkMode, UTSMode, IpcMode = ns:<path>
{"message":"network ns:/run/netns/nsdemo not found"}
$ create and start a container whose network path is a FIFO
{"message":"network ns:/run/nsdemo/fifo not found"}
```

After (this change, GitHub runner; pinned net 4026532273, uts 4026532337,
ipc 4026532338):

```text
$ start -a nsdemo-a
hostname=pod-host net:[4026532273] uts:[4026532337] ipc:[4026532338]
$ start -a nsdemo-b
hostname=pod-host net:[4026532273] uts:[4026532337] ipc:[4026532338]
$ create and start a container whose network path is a FIFO
Error response from daemon: run b1cca01d...: network namespace /run/nsdemo/fifo: not a namespace file
```

On macOS the paths are inside Kiri's VM, so a running "pod" container
(`--hostname pod-host`) provides them as `/proc/<its pid>/ns/*`, and the
FIFO sits in its root filesystem. After (this change):

```text
$ pod container: VM pid 60, hostname=pod-host net:[4026532195] uts:[4026532191] ipc:[4026532192]
$ start -a nsdemo-a
hostname=pod-host net:[4026532195] uts:[4026532191] ipc:[4026532192]
$ start -a nsdemo-b
hostname=pod-host net:[4026532195] uts:[4026532191] ipc:[4026532192]
$ create and start a container whose network path is a FIFO (/proc/60/root/fifo)
Error response from daemon: run c8a503bc...: network namespace /proc/60/root/fifo: not a namespace file
```

In every case both containers are in the pinned namespaces and have the
namespace's hostname, not the one the client sent, and the FIFO is refused
at start instead of blocking the agent. `main` refuses all three with
`not found` on both systems.

**`tests/e2e.sh`:**

```text
                         main                         this change
Mac (Apple silicon)      553 passed, 0 failed         553 passed, 0 failed
GitHub runner (Linux)    446 passed, 41 failed, 11 skipped (both, same checks)
Linux host               454 passed, 44 failed (both, same checks)
```

Every check passes on the Mac with and without the change. On Linux the
same checks fail on both sides: the Mac-only ones (Finder folders, Rosetta,
`kiri machine`), TTY checks without a terminal, and the machines' own
networking. No check fails with the change that passes on `main`.

**Speed check** (`scripts/perf-run.sh`, 400 runs each, on GitHub):
within the limits.

```text
                                   main        this change
docker run --rm alpine true, p50   19.418 ms   19.406 ms
GET /_ping, p50                    0.209 ms    0.207 ms
Daemon memory at rest              8.4 MiB     7.9 MiB
System calls per container         162         162
kiri binary                        6442 KiB    6446 KiB
kiri-init binary                   1669 KiB    1669 KiB
```

**Windows:** `kiri-win` builds and passes its tests on `windows-latest` on
both sides; it does not depend on the changed crates. The engine Windows
runs in WSL 2 is the static Linux build tested above. Not yet run end to
end on a Windows machine.

Tested on: Linux (Debian 13, Ubuntu 24.04), macOS 27 (Apple silicon),
Windows (build and unit tests).

### How you found it

- [x] I use Kiri and ran into this myself
- [ ] I found it by reading the code or comparing with Docker
- [ ] Someone else reported it (link the issue above)

AI tools used to write this change: yes. The implementation, its tests and
this description were written with an AI coding assistant (Claude, by
Anthropic) working under my direction. The testing described above was run
on my machines and on GitHub Actions.

### Checklist

- [ ] I have at most 3 pull requests open on Kiri, this one included
- [x] `cargo fmt`, `cargo clippy` and the tests pass (see the note on
  `cargo fmt` and the pre-existing test failures above)
- [ ] It works on macOS, Windows and Linux, or the change only touches one
  of them (macOS and Linux tested end to end; Windows engine tested as the
  static Linux build; see the issue)
