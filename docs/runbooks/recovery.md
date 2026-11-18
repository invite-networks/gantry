# Recovery

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.3.x with the Docker Engine API engine layer |
| Validation | Section 1 matches behavior tested on the reference host with the earlier CLI engine. Sections 2 to 4 are planned, not yet executed. |

Gantry keeps all pod state in tmpfs (`/run/gantry`, `/run/netns`) and
in container labels. Recovery is normally a fresh `gantry up`.

## 1. After a reboot

Trigger: host rebooted.

What happens: namespaces, links, DHCP agents and pod state are gone. The
containers still exist in their engines, but they point at pods that no
longer exist. Docker containers with a restart policy fail to start, with
the shim reporting that the pod namespace does not exist. That is
expected.

With the boot unit enabled (`gantry@<name>`), nothing needs doing: it
runs `gantry up -d --wait` once the network and engines are up.

Without it:

```sh
cd <project directory>
sudo gantry up -d
```

Expected: every pod reported as created, every container recreated (their
`io.gantry.pod-id` label no longer matches the new pod), DHCP pods bound
again with the same MAC and usually the same lease.

Failure handling:

* `engine` errors naming a socket: the engine is not up yet. Start it, or
  for Podman make sure `podman.socket` is enabled, then rerun.
* The boot unit failed: `journalctl -u gantry@<name>`, fix the cause,
  `systemctl restart gantry@<name>`.

## 2. After a crash or interrupted command

Trigger: `gantry` was killed during `up` or `down`, or the host lost
power.

```sh
cd <project directory>
sudo gantry up -d       # or: sudo gantry down
```

Stale namespaces are replaced and partially created pods are rebuilt. If a
lock error appears, check that no other `gantry` is running for the
project (`ps -ef | grep gantry`); the lock is a `flock` and is released
when its holder exits.

## 3. A pod lost its network

Trigger: the parent interface went down or was recreated (for example a
VLAN interface restarted), and pods lost connectivity.

Child links do not survive their parent being deleted. Recreate the pods:

```sh
sudo gantry down
sudo gantry up -d
```

Use `gantry up -d --force-recreate` if only containers need replacing.

## 4. Manual cleanup

Trigger: the project file is gone or broken, so `gantry down` cannot run,
and leftovers must be removed.

1. Remove containers by label on each engine:

   ```sh
   P=<project>
   for s in /var/run/docker.sock /run/podman/podman.sock /run/kiri/docker.sock; do
     [ -S "$s" ] || continue
     curl -s --unix-socket "$s" \
       "http://d/containers/json?all=true&filters=%7B%22label%22%3A%5B%22io.gantry.project%3D$P%22%5D%7D" \
       | grep -o '"Id":"[0-9a-f]*"' | cut -d'"' -f4 \
       | while read -r id; do
           curl -s -X DELETE --unix-socket "$s" "http://d/containers/$id?force=true&v=true"
         done
   done
   ```

2. Stop DHCP agents for the project (they release their leases on SIGTERM):

   ```sh
   for f in /run/gantry/$P/pods/*/dhcp-*.pid; do
     [ -f "$f" ] && kill "$(cat "$f")"
   done
   ```

3. Remove namespaces and state:

   ```sh
   for ns in $(ip netns list | awk '{print $1}' | grep "^gt-$P-"); do ip netns del "$ns"; done
   find /run/gantry/$P -name uts -o -name ipc | xargs -r umount
   rm -rf /run/gantry/$P
   ```

Expected: no `gt-<project>-` namespaces, no `/run/gantry/<project>`, no
labeled containers. Pod names that start with the same prefix as another
project's pods can match the `grep`; check the list before deleting.

## 5. Rolling back a Gantry version

See [upgrade](upgrade.md#rollback).
