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
| Speed check: main versus the change, `scripts/perf.py` limits (CONTRIBUTING) | Runs automatically on the pull request. Not run locally: it cross-builds both versions in containers. |
| Every feature ships on macOS, Windows and Linux together, built and tested on each; no OS gate (CONTRIBUTING) | **Open.** The code has no OS gate, but it was only built and tested on Linux. On macOS and Windows the path refers to Kiri's Linux VM. Raised as a question in the issue. Needs a macOS run (your Mac) and a Windows machine before the PR. |
| `python3 scripts/ui-test.py` passes (CONTRIBUTING) | The change has no window or UI. It runs in CI on every PR. **Not run locally.** |
| `cargo fmt`, `cargo clippy` and the tests pass (CONTRIBUTING, PR template) | Lines the change adds or edits are rustfmt-clean (stable rustfmt 1.11). Upstream `main` itself is not: `cargo fmt --check` reports hundreds of diffs in untouched files, so the change does not reformat them (one change per PR). Clippy reports no new warnings. Unit tests: 6 new, all pass; the 6 failures also fail on `main` (unprivileged test run). `tests/e2e.sh`: identical results on `main` and the change. See Part 2. |
| Tests run before submitting (CONTRIBUTING: `cargo test`, `tests/e2e.sh`) | Done on Linux, `main` and the change compared. A final e2e run on `414a5bc` is in progress; the comparison shown is from `6d3f096`, which differs only by the FIFO check (not exercised by e2e). |
| `scripts/ci/notices.sh` after dependency changes (CONTRIBUTING) | Not applicable: no dependency changes. |
| Sign your commits (CONTRIBUTING) | **Needs you.** The commit is unsigned. Re-sign with your key before pushing (`git commit --amend -S --no-edit`). |
| Contributor License Agreement with Canestack Labs via CLA Assistant (CONTRIBUTING, CLA.md) | **Needs you.** Section 3 asks you to confirm you may grant the licence (employer rights) and that the contribution is your original work. See the AI note below. |
| Apache-2.0 licence for contributions (CONTRIBUTING) | Accepted by submitting. |
| At most 3 open pull requests (CONTRIBUTING, PR template) | **Check** your open PRs on the repo before opening. |
| Disclose AI tools used (PR template) | Drafted as "yes". **Edit to match what you want to state.** |
| How you found it (PR template) | "I use Kiri and ran into this myself". |
| Tested on (PR template) | Linux (Debian 13, kernel 6.12, x86_64). macOS and Windows pending. |
| Code of Conduct: professional, respectful, no one's private information | Drafts name no people and contain no addresses, host names, internal IPs or emails. Commit author is the GitHub noreply address. |
| SECURITY.md: vulnerabilities go through private reporting, not public issues | Nothing here is a vulnerability in released Kiri. The FIFO hang was in this unreleased patch and is fixed. The issue discusses the feature's security properties openly, which is normal design discussion. If a maintainer considers joining arbitrary namespaces a security concern, move that discussion to private reporting. |

### Open decisions for you

1. **Name the project or not.** The drafts describe the use case generically
   ("a pod manager that pins namespaces itself"). Gantry is not public;
   naming it is your call.
2. **AI disclosure wording** and the CLA's "original work" statement.
3. **Which account and fork** opens the PR (personal or invite-networks).
4. **macOS and Windows testing** before the PR (the issue asks the
   maintainers what they need first).

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

I have a working implementation on Linux (about 400 lines with tests,
mostly in the agent's container setup and the daemon's create path, no new
dependencies). Before opening a pull request I'd like to agree on two
things:

1. **macOS and Windows.** There, containers run in Kiri's Linux VM, so the
   path would name a namespace inside the VM, the same as Podman machine or
   Docker Desktop. The code has no OS gate, but nothing in the VM creates
   such namespaces for users today, so it is mainly useful on Linux hosts.
   Is that acceptable under "every feature ships on all three systems", and
   what testing would you want on macOS and Windows?
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

On Linux (Debian 13, kernel 6.12, x86_64), with the daemon built from
`main` and from this change.

Unit tests (`cargo test -p kiri -p kiri-init -p kiri-proto`):

```text
                 main (193dfd7)              this change
kiri             233 passed, 4 ignored       237 passed, 4 ignored
kiri-init         58 passed, 6 failed         60 passed, 6 failed
kiri-proto         5 passed                    5 passed
```

The 6 `kiri-init` failures (`layers` and `dns` tests) fail the same way on
`main`: they need privileges the test run did not have.

Before and after, a pinned network, UTS and IPC namespace and two
containers joining them through the API:

Setup (as root):

```sh
ip netns add nsdemo && ip -n nsdemo link set lo up
mkdir -p /run/nsdemo && touch /run/nsdemo/uts /run/nsdemo/ipc
unshare --uts=/run/nsdemo/uts --ipc=/run/nsdemo/ipc hostname pod-host
mkfifo /run/nsdemo/fifo
```

Each container is created through the API with `"Hostname": "ignored"`,
`"NetworkMode": "ns:/run/netns/nsdemo"`, `"UTSMode": "ns:/run/nsdemo/uts"`,
`"IpcMode": "ns:/run/nsdemo/ipc"`, and prints its hostname and
`readlink /proc/self/ns/{net,uts,ipc}`. The pinned files are net
4026532401, uts 4026532482, ipc 4026532483.

Before (`main`):

```text
$ create nsdemo-a with NetworkMode, UTSMode, IpcMode = ns:<path>
{"message":"network ns:/run/netns/nsdemo not found"}
$ create nsdemo-b with NetworkMode, UTSMode, IpcMode = ns:<path>
{"message":"network ns:/run/netns/nsdemo not found"}
$ create and start a container whose network path is a FIFO
{"message":"network ns:/run/nsdemo/fifo not found"}
```

After (this change):

```text
$ start -a nsdemo-a
hostname=pod-host net:[4026532401] uts:[4026532482] ipc:[4026532483]
$ start -a nsdemo-b
hostname=pod-host net:[4026532401] uts:[4026532482] ipc:[4026532483]
$ create and start a container whose network path is a FIFO
Error response from daemon: run 64f2aac6...: network namespace /run/nsdemo/fifo: not a namespace file
```

Both containers are in the pinned namespaces and have the namespace's
hostname, not the one the client sent. The FIFO is refused at start
instead of blocking the agent.

`tests/e2e.sh` against the daemon, `main` and this change:

```text
main (193dfd7):  454 passed, 44 failed, 0 skipped
this change:     454 passed, 44 failed, 0 skipped
```

The same 44 checks fail on both, so the change adds no failures. They
come from the test machine, not from Kiri: the suite expects a Mac
(Finder folders, `kiri machine` checks), an interactive terminal (TTY
checks), the default 172.17.0.0/16 bridge (taken by Docker on this host)
and open internet access (this network only allows DNS to one resolver).

Tested on: Linux (Debian 13). <macOS and Windows: pending, per the issue>

### How you found it

- [x] I use Kiri and ran into this myself
- [ ] I found it by reading the code or comparing with Docker
- [ ] Someone else reported it (link the issue above)

AI tools used to write this change: yes. The implementation, tests and this
description were written with an AI coding assistant (Claude) under my
direction, and reviewed and tested by me. <edit to match what you want to
state>

### Checklist

- [ ] I have at most 3 pull requests open on Kiri, this one included
- [x] `cargo fmt`, `cargo clippy` and the tests pass (see the note on
  `cargo fmt` and the pre-existing test failures above)
- [ ] It works on macOS, Windows and Linux, or the change only touches one
  of them
