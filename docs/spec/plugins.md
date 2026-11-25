# Plugins and Group Access (proposal)

Status: proposal for review. Once accepted, the requirements, design and test
plan pick up their parts and this file becomes the plugin reference.

## Goals

- Let separately installed programs react to gantry lifecycle events and
  supply pod addresses, without gantry knowing what they do.
- A host with no plugins behaves exactly as it does today.
- A plugin is started only when an event it subscribed to fires.
- Plugins can be written in any language. The first one, gantry-nautobot, is
  its own public repository and package.
- Members of the `gantry` group can run gantry without typing sudo.

Non-goals for the first release: plugins that change container specs,
plugins inside the gantry process, and a plugin registry.

## Install Layout

| Path | Contents |
|---|---|
| `/usr/lib/gantry/plugins/<name>/plugin.toml` | Manifest, installed by the plugin's package. |
| `/usr/local/lib/gantry/plugins/<name>/plugin.toml` | Manifest for a plugin installed by hand. Wins over `/usr/lib` on a name clash. |
| `/etc/gantry/plugins.toml` | Optional. Gantry's own per host plugin overrides: disable a plugin on this host, or change a hook's mode. |
| `/etc/gantry/plugins/<name>.toml` | Host settings for the plugin, such as a URL and token. Root owned, mode 0600. Gantry never reads it; the plugin does. |

`gantry plugin ls` lists the plugins found, their hooks and whether each is
enabled for the project in the current directory.

## Manifest

```toml
api = 1                        # protocol version gantry must speak
name = "nautobot"
version = "0.1.0"
exec = ["bin/gantry-nautobot"] # relative to the manifest's directory

[hooks]
post_up   = { mode = "sync",  timeout = "30s" }
post_down = { mode = "async", timeout = "120s" }

[address]                      # this plugin can allocate addresses
from = "nautobot"              # matches `address: { from: nautobot }`
timeout = "15s"
```

Gantry reads manifests at startup, which is a few small file reads. A plugin
whose manifest has an unknown `api`, a missing `exec` or a bad hook name is
skipped with a warning.

`mode` is the plugin's default. A host can override it per hook in
`/etc/gantry/plugins.toml` (`[nautobot.hooks] post_up = "async"`), since the
host owner decides whether `up` waits.

## Hooks

| Hook | When |
|---|---|
| `pre_up` | After the project loads and locks, before any image pull. |
| `post_up` | After every selected container is created or kept, and after `--wait`. |
| `pre_down`, `post_down` | Around `gantry down`. `post_down` lists what was removed. |
| `post_start`, `post_stop` | After `gantry start` and `gantry stop`. |
| `post_restart` | After `gantry restart`. |
| `heal_restart` | After `gantry supervise` restarts or recreates an unhealthy container. |
| `boot` | After `gantry boot` brings projects up at system start, once per project. |

Every hook is notification only and fails open. A plugin that crashes, times
out or reports an error produces a warning, and the command carries on with
its normal exit status. `sync` waits for the plugin's reply or the timeout.
`async` hands the event to the plugin and does not wait.

Address allocation is the one call that fails closed, see
[Addresses](#addresses).

The first release of gantry-nautobot subscribes to `post_up` and
`post_down` only.

## Protocol

Gantry starts a plugin at most once per command, at the first event that
needs it. It talks over the plugin's stdin and stdout, one JSON object per
line. The plugin's stderr goes to gantry's log, prefixed with the plugin
name.

```text
gantry -> {"type":"hello","api":1,"gantry":"0.2.0","host":"node1","lock":"/run/gantry/plugins/nautobot/shop.lock"}
gantry -> {"id":1,"type":"event","hook":"post_up","mode":"sync","deadline":"2026-11-25T18:00:30Z","data":{...}}
plugin -> {"id":1,"ok":true}
gantry -> {"id":2,"type":"allocate","data":{...}}
plugin -> {"id":2,"ok":true,"result":{"address":"192.0.2.218/29","gateway":"192.0.2.222"}}
plugin -> {"id":3,"ok":false,"error":"no free address in 192.0.2.216/29"}
```

- Gantry closes stdin after its last event. The plugin finishes what is
  queued and exits.
- The plugin starts in its own session (`setsid`), so it outlives gantry and
  finishes async events after gantry exits. It must ignore a broken stdout
  pipe and stop by the last `deadline` it received.
- `lock` is a per project, per plugin file. The plugin holds `flock` on it
  while it works, so a late async `post_down` cannot land after the next
  `up`'s `post_up`. The next plugin process for the same project waits for
  the lock.
- Plugins run as root with gantry's environment cleared, except `PATH`,
  `LANG` and `GANTRY_PLUGIN_*`.

## Event Data

Each event carries the project as gantry sees it after the step finished.

```json
{
  "project": "shop",
  "compose_files": ["/srv/shop/compose.yaml"],
  "settings": { "tenant": "acme" },
  "pods": [{
    "name": "web",
    "netns": "gt-shop-web",
    "hostname": "web",
    "state": "running",
    "interfaces": [{
      "name": "eth0", "parent": "vlan4", "driver": "macvlan",
      "address": "192.0.2.218/29", "gateway": "192.0.2.222",
      "address6": null, "gateway6": null, "default_route": true,
      "mac": "02:42:c0:00:02:da", "from": null
    }],
    "containers": [{
      "service": "nginx", "name": "shop-nginx-1", "engine": "docker",
      "id": "4f1c...", "image": "nginx:alpine", "digest": "sha256:...",
      "state": "running", "restart": "unless-stopped", "restart_count": 0,
      "command": ["nginx", "-g", "daemon off;"],
      "cpus": 1.0, "mem_limit": 2147483648, "labels": {},
      "volumes": [{ "type": "volume", "name": "shop_data", "source": null, "target": "/data", "read_only": false }]
    }]
  }]
}
```

Environment variables, secrets and registry credentials are never sent.
`settings` is the project's `plugins.<name>` block with `enabled` removed.

## Compose Keys

```yaml
plugins:
  nautobot:
    enabled: false        # opt out; every installed plugin runs by default
    tenant: acme          # anything else is passed to the plugin as settings

pods:
  web:
    network:
      - parent: vlan4
        address: { from: nautobot, prefix: 192.0.2.216/29 }
```

A `plugins:` entry for a plugin that is not installed is a warning, not an
error, so one compose file works on hosts with and without it.

## Addresses

`address: { from: <name>, ... }` asks the plugin whose manifest declares
`[address] from = "<name>"` for the address. Every key besides `from` goes to
the plugin untouched, together with the rest of the network entry (`parent`,
`driver`, `gateway`, `interface`, `default_route`, `address6`, `gateway6`),
the project, the pod and the `plugins.<name>` settings.

- Gantry asks only when it creates the pod. A pod whose configuration did not
  change keeps the address recorded in its state, so a repeated `up` makes
  no call.
- The reply sets `address` and may set `gateway`. A gateway in the compose
  file wins over the plugin's.
- This call fails closed. If the plugin is missing or disabled, errors or
  times out, the pod is not created and `up` fails with the plugin's
  message.
- `guard_addresses` skips static conflict checks for these entries. The
  plugin owns uniqueness.
- `gantry config` prints `from: nautobot` and, once the pod exists, the
  address it was given.
- The off-Linux parser used by gantry-server must accept the map form.

## gantry-nautobot

Public repository `invite-networks/gantry-nautobot`, its own .deb, targeting
invite-nautobot 0.4.0 or later.

| Gantry | invite-nautobot |
|---|---|
| Pod | Pod with the gantry pod name and `compose_project` set, found with `?name=&compose_project=&cluster=` (`cluster=null` without one), `node` from the host name (Device or Virtual Machine), `cluster` from the node's Container Cluster when it has one. |
| Pod interface | Container Interface on the Pod, its IP Address attached. The Container Network is the one whose Prefix holds the address. |
| Container | Container with `type` docker or kiri, `pod` set, no cluster or node of its own, `compose_project` and `compose_service` set, found with `?pod=<id>&name=`. |
| Volume | Container Volume on the container. |
| `down` | Pods and their containers get `state: down`; State Changed records when. Cleanup is a later job keyed on that time. |
| `from: nautobot` | `POST /api/ipam/prefixes/<id>/available-ips/`. The prefix comes from `prefix:` or, without one, from the parent: the Prefix holding the parent's own host address, else the Prefix of the cluster's Container Network on the parent's VLAN ID (read from the kernel, not parsed from the name). Before allocating, the pod's existing interface address is reused when no live owner holds it. |

Records are identified by scope, not by a global name: a Pod by cluster,
Compose project and name, a container in a pod by pod and name. The node is
not part of identity, so when a project moves to another host its records
move with it. Two hosts in the same cluster running the same project at once
would share records, so that is not supported.

## Group Access

The package already creates a `gantry` system account and group for remote
control. `/etc/sudoers.d/gantry` also gives people in that group gantry as
root, excluding the remote control account itself:

```text
User_Alias GANTRY_USERS = %gantry, !gantry
Defaults:GANTRY_USERS !env_reset
GANTRY_USERS ALL=(root) NOPASSWD: /usr/bin/gantry
```

When `/usr/bin/gantry` runs as a non-root member, it replaces itself with
`sudo -n /usr/bin/gantry ...`, keeping its arguments and working directory.
`completion` and `internal man` run without elevating.

The environment is kept, as for a Docker CLI user, so compose interpolation,
`~/` paths and the member's registry login behave as they expect. sudo still
removes its unsafe variables and applies `secure_path`. The remote control
account keeps `env_reset`.

A non-member without root gets the current error, plus a hint to join the
group.

**Security:** the `gantry` group is equivalent to root, as the `docker`
group is. A compose file can bind mount `/` or run privileged containers.
The install guide says so.

## Phases

1. Group access (done).
2. Plugin discovery, manifest, protocol and hooks, with a test plugin in
   `tests/` that records what it receives.
3. Address allocation through plugins.
4. gantry-nautobot in its own repository, once invite-nautobot 0.4.0 ships.
