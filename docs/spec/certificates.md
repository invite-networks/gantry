# Pod Certificates (proposal)

Status: proposal for review. Names marked *(pending)* are waiting on the
internal CA's owner to confirm them.

## Goals

- A pod that needs a TLS certificate from the internal CA gets one when it is
  created, renewed before it expires, in the files and formats each service
  expects, without the application image knowing about the CA.
- A service is told when its certificate changes, in the way it understands:
  a signal, a command, a restart, or nothing.
- A project moved to another host gets its certificate there. Nothing is
  copied between hosts.
- Gantry does not depend on one CA. The CA is reached through an issuer
  plugin. The first one is for step-ca.

Not in the first release: IP addresses in certificates, client certificates
for mutual TLS between pods, revocation, and certificates for projects that
are not on a Gantry Host.

## How a Host Asks for Certificates

Every Gantry Host holds a **Gantry credential**, provisioned by Ansible (role
`step_ca`, playbook `playbooks/gantry-hosts.yml`) only on hosts whose
Nautobot role is "Gantry Host":

| | |
|---|---|
| CA | `https://infraca.infra.invite.global` |
| Credential | `/etc/ssl/gantry/credential.crt` (leaf and chain), SAN `<host>.infra.invite.global` |
| Key | `/etc/ssl/private/gantry-credential.key`, root only, 0600 |
| CA root | `/usr/local/share/ca-certificates/infraca-root.crt` |
| Renewal | a systemd timer runs `step ca renew`, and the next Ansible run reissues a lapsed credential |

Gantry sends the pod's certificate request to the X5C provisioner
`gantry-hosts` *(pending)*, signed with the credential. That provisioner
accepts only Gantry credentials, not ordinary host certificates, and issues
DNS names under `infra.invite.global` only, for 30 days.

The ordinary host certificate (`/etc/ssl/host/`) is never used: its key is
readable by the `ssl-cert` group, and every Linux host has one.

## Project File

```yaml
pod:
  tls:
    names: [app.infra.invite.global]   # extra names, see Names
    key_type: ec256                    # ec256 (default), rsa2048, rsa4096
    optional: false                    # true: start without a certificate and keep trying

services:
  web:                                  # defaults: /etc/tls with cert.pem, key.pem, ca.pem
    tls: { on_renew: { signal: SIGHUP } }

  api:
    tls:
      dir: /app/certs
      files:
        tls.crt: fullchain
        tls.key: key
        ca.crt: root
      on_renew: restart

  haproxy:
    tls:
      dir: /usr/local/etc/haproxy/certs
      files: { site.pem: bundle }
      on_renew: { exec: [kill, -USR2, "1"] }

  keycloak:
    tls:
      dir: /opt/tls
      owner: "1000:1000"
      files:
        keystore.p12: { format: pkcs12, password: "${KEYSTORE_PASSWORD}" }
```

`pod.tls` turns certificates on for the pod. A service gets the files only
when it has its own `tls:` block. A service with `tls:` in a pod without one
is an error.

### Names

The certificate carries the pod's hostname as a fully qualified name under
`infra.invite.global` (`<hostname>.infra.invite.global` unless `domainname:`
already gives one), plus every entry in `names`. A name outside
`infra.invite.global` is a configuration error, caught before the CA is
asked. IP addresses are never included, so clients connect by name. The
pod's DNS records come from Nautobot (gantry-nautobot records the pod, and
invite-nautobot publishes its names).

### Files

| Content | What it holds |
|---|---|
| `cert` | the pod's certificate |
| `chain` | the intermediates |
| `fullchain` | certificate and intermediates (what most servers want) |
| `key` | private key, PEM, PKCS#8 |
| `key-pkcs1` | private key in the older RSA or EC form |
| `root` | the internal CA root |
| `bundle` | key and full chain in one PEM |
| `pkcs12` | key and chain in one PKCS#12 file, with `password:` |

A value is a content name, or a map with `format:` (`pem`, the default,
`der`, or `pkcs12`) and, for PKCS#12, `password:`. Without `files:` a service
gets `cert.pem` (full chain), `key.pem` and `ca.pem`.

### Ownership

Files are owned by `owner:`, or by the service's numeric `user:`, or by root.
Key material (`key`, `key-pkcs1`, `bundle`, `pkcs12`) is mode 0400, the rest
0444. An image whose user is only a name needs `owner:`.

### On Renewal

| `on_renew` | What gantry does |
|---|---|
| `{ signal: SIGHUP }` | sends the signal to the container's main process |
| `{ exec: [cmd, ...] }` | runs the command in the container |
| `restart` | restarts the container (the default) |
| `none` | nothing, for a service that reloads the files itself |

## How It Works

1. **Issue.** When gantry creates a pod with `tls:`, it generates the key on
   the host, builds the request with the pod's names, and asks the issuer
   plugin to sign it. Without `optional: true`, a failure stops the pod from
   being created, as a failed address allocation does.
2. **Store.** The key and certificate live under the pod's directory in
   `/run/gantry`, in memory. They are never written to the project directory
   or NFS, and a reboot or a move gets a fresh certificate.
3. **Render.** Each service's files are written to its own directory under
   the pod's, in its layout and ownership, and that directory is mounted
   read-only at `dir`. Gantry mounts the directory, not single files, so a
   renewed file replaced by rename is seen in the container. A file mount
   would keep the old copy.
4. **Renew.** The watcher that already acts on health (`gantry supervise`, or
   gantry-agent through the library) renews a certificate at two thirds of
   its lifetime, re-renders every service's files at once, and runs each
   service's `on_renew`. A failed renewal keeps the current certificate,
   warns, and retries with backoff.
5. **Show.** `gantry ps` and `gantry stats` show each pod's certificate
   expiry, and plugins get it in their event data, so monitoring can alert
   before a certificate runs out.

## Issuer Plugin

Signing goes through the plugin protocol, as address allocation does. A
plugin declares `[certificate] from = "stepca"` in its manifest and answers
`sign` requests:

```json
{ "type": "sign", "id": 7, "data": { "project": "shop", "pod": "web",
  "csr": "-----BEGIN CERTIFICATE REQUEST-----...", "names": ["web.infra.invite.global"],
  "lifetime_hours": 720 } }
```

The answer carries the certificate, the chain and the root, in PEM. The
first plugin, **gantry-stepca**, signs with the Gantry credential through
the `gantry-hosts` provisioner. Its settings file names the CA URL, the
provisioner and the credential paths. Without a project setting, a pod uses
the only installed issuer, and more than one installed issuer means the pod
must name one (`tls: { issuer: stepca }`).

## Moving and Removing

- **Move.** A project started on another host creates its pod there and gets
  a new certificate from that host's credential. The old host's copy goes
  with its pod.
- **Down.** `gantry down` removes the key and certificate with the pod's
  directory. The certificate stays valid at the CA until it expires (up to
  30 days). Revocation at `down` can be added if that window matters.

## Open Questions

1. Should `on_renew` default to `restart`, which always works but costs a
   restart every 20 days, or to `none`, which is free but silently wrong for
   software that does not reload its certificates?
2. Is the full chain or the leaf alone the right default for `cert.pem`?
   Full chain suits servers. Some clients want the leaf alone.
3. Does any service need the certificate before its pod exists, for example
   a build step? This design does not cover that.
