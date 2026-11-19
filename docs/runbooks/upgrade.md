# Upgrade and rollback

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.1.x |
| Validation | Planned, not yet executed. |

## Upgrade

Trigger: a new Gantry version is to be installed.

Prerequisites: the new binaries built or downloaded; the previous binaries
kept for rollback; release notes read for format or state changes.

1. Keep the current binaries:

   ```sh
   sudo cp /usr/local/bin/gantry /usr/local/bin/gantry.prev
   sudo cp /usr/local/bin/gantry-oci /usr/local/bin/gantry-oci.prev
   ```

2. Install the new ones. Running pods and containers are not affected; the
   shim is only called when Docker creates or restarts a container.

   ```sh
   sudo install -m 755 target/release/gantry target/release/gantry-oci /usr/local/bin/
   ```

3. Rerun the engine setup that applies to the host. Both are idempotent.

   ```sh
   sudo gantry setup docker
   ```

4. Validate each project without changing it, then apply:

   ```sh
   cd <project directory>
   sudo gantry config >/dev/null
   sudo gantry up -d
   ```

Expected: `config` succeeds. `up` reports containers as up to date, or
recreates them once if the new version changes how containers are created
(the config hash changes).

Running DHCP agents are the old binary until their pod is recreated. To move
them to the new version, run `gantry down` and `gantry up -d` in a
maintenance window.

## Moving from the 0.2 or 0.3 test releases

Gantry was re-released as 0.1.0 before any production use, so apt sees
0.1.0 as older than the earlier 0.2.x and 0.3.x test packages. Install it
once with a downgrade, after which normal upgrades resume:

```sh
sudo apt update
sudo apt install --allow-downgrades gantry=0.1.0
gantry version                     # gantry 0.1.0, features: health_on_failure, supervise
```

Projects that set `runtime: podman` no longer load: move them to
`docker` or `kiri` first.

## Rollback

Trigger: the new version fails validation.

```sh
sudo install -m 755 /usr/local/bin/gantry.prev /usr/local/bin/gantry
sudo install -m 755 /usr/local/bin/gantry-oci.prev /usr/local/bin/gantry-oci
cd <project directory>
sudo gantry up -d
```

State under `/run/gantry` has no version marker. If the older version
cannot read it, run `gantry down` with the newer version first, roll
back, then `up`.
