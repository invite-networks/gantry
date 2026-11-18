# Install, engine setup and uninstall

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.3.x with the Docker Engine API engine layer |
| Validation | On the Debian 13 reference host (2026-11-16): the .deb installed with `apt`, and a pod with Docker, Podman and kiri services ran through the API engine layer. The .deb was built locally, not fetched from the apt repository. Steps 3 to 5 were done on that host before the .deb existed. |

## 1. Prerequisites

* Linux, kernel 6.x, root access. Reference: Debian 13, kernel 6.12.
* `nftables` (the `nft` command), for pods with a firewall.
* `iproute2`, for operating and troubleshooting (`ip netns`, `ip -n`).
* A parent interface for the pods, such as a VLAN interface, with a
  subnet and gateway on the LAN, and free addresses or a DHCP server.
* At least one engine: Docker Engine, Podman 5, or kiri 0.2 with the
  `contrib/kiri` patch.
* To build Gantry from source (section 2b): a Rust toolchain matching
  `rust-toolchain.toml`.
* For projects that use `build:`: the `docker` CLI (Docker and kiri) or the
  `podman` CLI.

Apart from image builds, the `docker` and `podman` command line tools are
not needed. Gantry talks to each engine's API socket.

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
`gantry@.service` unit, the man page and shell completions. It also sets up
the restricted `gantry` account for remote control over SSH, which cannot
log in until a key is added; see [remote](remote.md).

### 2b. From source

For hosts without the package, or to test a change:

```sh
cargo build --release
sudo install -m 755 target/release/gantry target/release/gantry-oci /usr/local/bin/
sudo install -m 644 contrib/systemd/gantry@.service /etc/systemd/system/
sudo systemctl daemon-reload
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

## 4. Set up Podman

Trigger: the host runs Podman services.

```sh
sudo gantry setup podman
```

This enables `podman.socket` (systemd socket activation), which serves
Podman's Docker-compatible API at `/run/podman/podman.sock`. It is the same
as `sudo systemctl enable --now podman.socket`.

Validate:

```sh
sudo curl -s --unix-socket /run/podman/podman.sock http://d/_ping
```

Expected: `OK`.

If the socket does not answer, Gantry runs `systemctl start podman.socket`.
If that is unavailable, it starts a private
`podman system service --time=300` on `/run/gantry/podman.sock` (mode
0600), which exits after 5 idle minutes. The fallback needs the `podman`
binary and adds startup time, so the socket is the recommended setup.

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

Pods live under `/run`, so they are gone after a reboot. The
`gantry@.service` template unit brings a project up at boot and down at
shutdown. Each project is a directory under `/etc/gantry`:

```sh
sudo install -d -m 0755 /etc/gantry/myproject
sudo install -m 0644 gantry.yaml /etc/gantry/myproject/
sudo install -m 0600 .env /etc/gantry/myproject/      # if the project has one
sudo systemctl enable --now gantry@myproject
```

Keep the project directory for the project file and `.env` only, owned by
root. Put container data elsewhere, on storage chosen for it, and bind mount
it by absolute path:

```yaml
services:
  db:
    volumes:
      - ${DATA_DIR}/db:/var/lib/postgresql/data
```

with `DATA_DIR=/srv/myproject` (or wherever the host keeps data) in the
project's `.env`. Data then survives the project being replaced, backups can
cover one data tree, and remote control's ownership checks stay simple.
Prefer bind mounts to named volumes, which hide data inside the engine.

The unit orders itself after `network-online.target`, `docker.service`,
`podman.socket` and `kiri.service`.

Validate: `systemctl status gantry@myproject` is active, and
`cd /etc/gantry/myproject && sudo gantry pod ls` lists the pods.

## 7. Environment overrides

| Variable | Default | Use |
|---|---|---|
| `GANTRY_DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker API socket |
| `GANTRY_PODMAN_HOST` | `unix:///run/podman/podman.sock` | Podman API socket |
| `GANTRY_KIRI_HOST` | `unix:///run/kiri/docker.sock` | kiri API socket |
| `GANTRY_RUN_DIR` | `/run/gantry` | State directory |
| `GANTRY_NETNS_DIR` | `/run/netns` | Network namespace pins |
| `GANTRY_NFT` | `nft` | nftables CLI (debug builds only; release builds always use `nft` from `PATH`) |
| `GANTRY_LOG` | `info` | Log level |
| `GANTRY_OCI_RUNTIME` | `runc` | Runtime the shim hands off to (set in dockerd's environment) |

These are read by a root process. Do not configure sudo to keep the
caller's environment for Gantry.

## 8. Uninstall

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
2. Disable boot units:

   ```sh
   sudo systemctl disable --now 'gantry@*'
   ```
3. Remove the Docker runtime: delete the `gantry` entry under `runtimes`
   in `/etc/docker/daemon.json` and `sudo systemctl reload docker`. Do this
   only after every Gantry container is gone, or Docker cannot restart
   them.
4. Optionally disable the Podman socket if nothing else uses it:
   `sudo systemctl disable --now podman.socket`.
5. Remove the package and leftovers. `purge` also removes the `gantry`
   account, its keys and the sshd and sudoers drop-ins:

   ```sh
   sudo apt purge gantry
   # source installs instead:
   # sudo rm /usr/local/bin/gantry /usr/local/bin/gantry-oci /etc/systemd/system/gantry@.service
   # sudo systemctl daemon-reload
   ip netns list | grep '^gt-'          # should print nothing
   sudo rm -rf /run/gantry
   ```

Expected: no `gt-` namespaces, no `/run/gantry`, no containers with an
`io.gantry.project` label.
