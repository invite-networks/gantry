# Upgrade and rollback

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.3.x |
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
   sudo gantry setup podman
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

## From 0.2 to 0.3

0.3 moves networks into the pods and turns on a default firewall. Every
project file needs editing, and 0.3 refuses a 0.2 file with an error that
names the section to move. Do this per project, in a maintenance window:

1. Bring the project down with the 0.2 binary, before upgrading, so its
   DHCP leases are released: `sudo gantry down`.
2. Rewrite the file. For each pod, replace the `networks:` map with a
   `network:` list, taking `parent` from the old top-level network:

   ```yaml
   # 0.2
   networks:
     lan:
       driver_opts: { parent: vlan4 }
       ipam: { config: [ { subnet: 192.0.2.216/29, gateway: 192.0.2.222 } ] }
   pods:
     web:
       networks: { lan: { ipv4_address: 192.0.2.218 } }

   # 0.3
   pods:
     web:
       network:
         - parent: vlan4
           address: 192.0.2.218/29
           gateway: 192.0.2.222   # only when it is not the first host
   ```

   `dhcp: true` becomes `address: dhcp`. `ipv6_address` becomes
   `address6` with its prefix. Remove `mode`, `mtu` and `allow_ping`. Rules
   with `from: network:<name>` use `from: subnet`.
3. Review the firewall. A pod without a `firewall:` section was open in 0.2
   and now rejects its own subnet. A pod with one defaulted to input
   `drop` in 0.2 and now defaults to `accept` with the subnet reject. Set
   `policy: { input: drop }` to keep a closed pod closed.
4. Upgrade (below), then `sudo gantry config` to validate and
   `sudo gantry up -d`.

macvlan MAC addresses are now derived from the interface name instead of
the network name, so each macvlan pod gets a new MAC once. DHCP pods may get
a new lease, and static pods announce the new MAC with gratuitous ARP.

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
