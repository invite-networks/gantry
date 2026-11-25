# Install, engine setup and uninstall

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.1.x with the Docker Engine API engine layer |
| Validation | On the Debian 13 reference host (2026-11-16): the .deb installed with `apt`, and a pod with Docker and kiri services ran through the API engine layer. The .deb was built locally, not fetched from the apt repository. Steps 3 to 5 were done on that host before the .deb existed. |

## 1. Prerequisites

* Linux, kernel 6.x, root access. Reference: Debian 13, kernel 6.12.
* `nftables` (the `nft` command), for pod firewalls.
* `rsync`, for `gantry data move`. The package depends on both.
* `iproute2`, for operating and troubleshooting (`ip netns`, `ip -n`).
* A parent interface for the pods, such as a VLAN interface, with a
  subnet and gateway on the LAN, and free addresses or a DHCP server.
* At least one engine: Docker Engine (the primary engine), or kiri 0.2
  with the `contrib/kiri` patch.
* To build Gantry from source (section 2b): a Rust toolchain matching
  `rust-toolchain.toml`.
* For projects that use `build:`: the `docker` CLI (Docker and kiri).

Apart from image builds, the `docker` command line tool is not needed. Gantry talks to each engine's API socket.

## 2. Install Gantry

Trigger: new host, or first use on a host.

### 2a. From the apt repository (recommended)

Packages are built for amd64 on Debian and Ubuntu.

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://invite-networks.github.io/apt/invite-networks.asc |
  sudo tee /etc/apt/keyrings/invite-networks.asc >/dev/null
sudo tee /etc/apt/sources.list.d/invite-networks.sources >/dev/null <<'SRC'
Types: deb
URIs: https://invite-networks.github.io/apt
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/invite-networks.asc
SRC
sudo apt update
sudo apt install gantry
```

Validate: `gantry --version` prints the version.

The package installs `gantry` and `gantry-oci` in `/usr/bin`, the
`gantry-boot.service` unit (enabled), `/etc/gantry/project-dirs`, the man page and shell
completions. It creates `/mnt/container-local` (root owned, mode 0755) for
local project storage when it does not exist; an existing directory or
mountpoint is left as it is, and purging the package never removes it. It also sets up
the restricted `gantry-remote` account for remote control over SSH, which cannot
log in until a key is added; see [remote](remote.md).

### 2b. From source

For hosts without the package, or to test a change:

```sh
cargo build --release
sudo install -m 755 target/release/gantry target/release/gantry-oci /usr/local/bin/
sudo install -m 644 contrib/systemd/gantry-boot.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable gantry-boot.service
```

`cargo deb` builds the same .deb the repository serves. Do not mix the two
installs on one host: `/usr/local/bin` comes first in `PATH`, so a stale
source build hides the packaged one.

## 3. Set up Docker

Trigger: the host runs Docker services.

```sh
sudo systemctl enable --now docker
sudo gantry setup docker
```

`setup docker` adds the `gantry` runtime to `/etc/docker/daemon.json`
(other keys are kept) pointing at the `gantry-oci` next to the
`gantry` binary, then reloads `dockerd`. Options: `--oci-path`,
`--daemon-json`, `--no-reload`.

Validate:

```sh
sudo curl -s --unix-socket /var/run/docker.sock http://d/info | grep -o '"gantry"'
```

Expected: `"gantry"` is listed among the runtimes.

To hand off to a runtime other than `runc` (for example `crun`), set
`GANTRY_OCI_RUNTIME` in dockerd's environment with a systemd drop-in for
`docker.service`.

Failure handling: if dockerd does not reload, run
`sudo systemctl restart docker` and check `journalctl -u docker`. A JSON
error in `daemon.json` stops dockerd from starting; fix the file and
restart.

## 4. Self-healing watcher

Trigger: always; the package sets it up.

The package installs and enables `gantry-supervise.service`, which runs
`gantry supervise`. It restarts or stops containers that turn `unhealthy`
when their service sets `health_on_failure` (see the
[file format](../compose-spec.md#health_on_failure)). Services without the
key are never touched.

Validate:

```sh
systemctl is-active gantry-supervise.service    # active
journalctl -u gantry-supervise -n 5             # "health watcher started (docker, kiri)"
```

An engine that is not installed is skipped and checked again on each poll.
Only one watcher runs per host: it holds `/run/gantry/heal.lock`, and a
second `gantry supervise` exits with "another health watcher is running".
When gantry-agent manages the host it runs the watcher itself; take over
with `sudo systemctl disable --now gantry-supervise.service`. Upgrades keep
it disabled.

## 5. Set up kiri

Trigger: the host runs kiri services.

1. Install kiri 0.2 and apply `contrib/kiri/0001-network-mode-ns-path.patch`
   (see `contrib/kiri/README.md`). Stock kiri rejects `ns:<path>` network
   mode.
2. Run the patched daemon so it serves `/run/kiri/docker.sock`. A systemd
   drop-in for `kiri.service` that points `ExecStart` at the patched
   binary keeps packaged files untouched.
3. Validate:

```sh
sudo curl -s --unix-socket /run/kiri/docker.sock http://d/_ping
```

Expected: `OK`. If kiri listens elsewhere, set `GANTRY_KIRI_HOST`.

## 6. Run a project at boot

Pods live under `/run`, so they are gone after a reboot, and the engines'
restart policies cannot bring containers back without them.
`gantry-boot.service`, enabled by the package, runs `gantry boot start` at
boot and `gantry boot stop` at shutdown for every registered project.

A project is a directory holding `gantry.yaml`, its `.env` and usually its
data. Gantry finds it by name in the directories listed in
`/etc/gantry/project-dirs`, one parent per line, with `*` and `?` matching
within a path component. The package installs:

```
/etc/gantry
/mnt/*
```

so `myproject` can be `/etc/gantry/myproject/` or, next to its data on
another disk or an NFS mount, `/mnt/<storage>/myproject/`:

```sh
sudo install -d -m 0755 /mnt/storage/myproject
sudo install -m 0644 gantry.yaml /mnt/storage/myproject/
sudo install -m 0600 .env /mnt/storage/myproject/      # if the project has one
sudo gantry --installed myproject up -d                # starts and registers it
```

`gantry up -d` registers an installed project for boot (it prints
`Registered myproject to start at boot` the first time), and `gantry down`
removes it. `gantry stop` does not: a stopped project comes back at boot.
`up` without `-d`, or a project run from any other directory, is never
registered. Registrations are files in `/var/lib/gantry/boot/`.

Boot order comes from `priority` on pods, across every registered project
on the host: lower first, default 100, equal levels together. Each level
waits until its services are running, or healthy where they have a
healthcheck, for up to 5 minutes, then the next level starts. A project
that fails is reported and does not hold up the others. Service `priority`
orders services inside their pod the same way. `sudo gantry boot ls` shows
the order.

A name must exist in exactly one of those places; Gantry refuses a name it
finds twice. Symlinks are not followed. The same name
works by hand: `sudo gantry --installed myproject ps`.

Keep the project directory, its top-level files and every directory above
it owned by root and not writable by a group or others. Remote control
refuses anything else. On NFS that means the export must not squash root.
Data directories inside the project may be owned by the container's user.
Bind data with a path relative to the project, so it moves with it:

```yaml
services:
  db:
    volumes:
      - ./db/data:/var/lib/postgresql/data
```

Prefer bind mounts to named volumes, which hide data inside the engine.

The unit orders itself after `network-online.target`, `remote-fs.target`
(NFS mounts), `docker.service` and `kiri.service`.

Validate: `systemctl is-enabled gantry-boot.service` says `enabled`,
`sudo gantry boot ls` lists the project, and after a reboot
`journalctl -u gantry-boot` shows each priority level starting.

## 7. Environment overrides

| Variable | Default | Use |
|---|---|---|
| `GANTRY_DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker API socket |
| `GANTRY_KIRI_HOST` | `unix:///run/kiri/docker.sock` | kiri API socket |
| `GANTRY_RUN_DIR` | `/run/gantry` | State directory |
| `GANTRY_NETNS_DIR` | `/run/netns` | Network namespace pins |
| `GANTRY_NFT` | `nft` | nftables CLI (debug builds only; release builds always use `nft` from `PATH`) |
| `GANTRY_LOG` | `info` | Log level |
| `GANTRY_OCI_RUNTIME` | `runc` | Runtime the shim hands off to (set in dockerd's environment) |

These are read by a root process. Members of the `gantry` group (see
[Run gantry without sudo](#8-run-gantry-without-sudo)) keep their
environment, which is safe only because membership is already equivalent to
root. Do not keep the environment for any narrower sudo rule.

## 8. Run gantry without sudo

Trigger: a person should operate Gantry on this host without typing sudo.

```sh
sudo usermod -aG gantry alice     # then alice logs out and back in
```

A member runs `gantry up`, `gantry ps` and the rest directly. Gantry runs
itself as root through `sudo -n`, allowed by `/etc/sudoers.d/gantry`, with
the member's environment kept so compose interpolation, `~/` paths and their
Docker registry login work as they would for them. This applies only to the
installed `/usr/bin/gantry`.

**The `gantry` group is equivalent to root**, as the `docker` group is: a
compose file can bind mount `/` or run privileged containers. Add only people
you would give root. The remote control account is `gantry-remote`, in its
own group, and is not affected.

Validate as the member: `gantry ls` runs without a password prompt, and
`sudo -n id` is still refused.

## 9. Uninstall

Trigger: removing Gantry from a host.

1. Bring every project down, so containers, namespaces and DHCP leases are
   released:

   ```sh
   ls /run/gantry                    # projects that are up
   cd <project directory>
   sudo gantry down                  # for each project; add -v to drop named volumes
   ```

   `down` reads the project file. If it is gone, use the manual cleanup in
   [recovery](recovery.md#4-manual-cleanup).
2. `down` also removed each project from boot. Stop the boot unit:

   ```sh
   sudo systemctl disable --now gantry-boot.service gantry-supervise.service
   ```
3. Remove the Docker runtime: delete the `gantry` entry under `runtimes`
   in `/etc/docker/daemon.json` and `sudo systemctl reload docker`. Do this
   only after every Gantry container is gone, or Docker cannot restart
   them.
4. Remove the package and leftovers. `purge` also removes the `gantry`
   account, its keys and the sshd and sudoers drop-ins:

   ```sh
   sudo apt purge gantry
   # source installs instead:
   # sudo rm /usr/local/bin/gantry /usr/local/bin/gantry-oci /etc/systemd/system/gantry-boot.service
   # sudo systemctl daemon-reload
   ip netns list | grep '^gt-'          # should print nothing
   sudo rm -rf /run/gantry
   ```

Expected: no `gt-` namespaces, no `/run/gantry`, no containers with an
`io.gantry.project` label.
