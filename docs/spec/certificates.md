# Pod Certificates (proposal)

Status: proposal for review. The CA side (`gantry-hosts-enroll` and
`gantry-hosts` on infraca) is built and tested but not yet in production.

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
| Credential | `/etc/ssl/gantry/credential.crt` (leaf and chain), client authentication only, SAN `<host>.infra.invite.global`, 90 days |
| Key | `/etc/ssl/private/gantry-credential.key`, root only, 0600 |
| CA root | `/usr/local/share/ca-certificates/infraca-root.crt` |
| Renewal | a systemd timer runs `step ca renew`, and the next Ansible run reissues a lapsed credential |

Gantry asks the X5C provisioner `gantry-hosts` for the pod's certificate,
signed with the credential, the equivalent of:

```sh
step ca certificate app.infra.invite.global pod.crt pod.key \
  --provisioner gantry-hosts \
  --x5c-cert /etc/ssl/gantry/credential.crt \
  --x5c-key /etc/ssl/private/gantry-credential.key
```

That provisioner accepts only credentials issued by `gantry-hosts-enroll`,
refusing ordinary host certificates and pod certificates alike, and issues
DNS names only, for 720 hours. **Renewal does not use the credential:** a pod
certificate is renewed with itself over mutual TLS (`step ca renew`), so a
running pod keeps its certificate even if its host's credential lapses.

The ordinary host certificate (`/etc/ssl/host/`) is never used: its key is
readable by the `ssl-cert` group, and every Linux host has one.

## Project File

```yaml
pod:
  tls:
    names: [shop.infra.invite.global]  # required, one label each, see Names
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

The CA allows exactly one label under `infra.invite.global`:
`app.infra.invite.global` is allowed, `web.app.infra.invite.global` and
`infraca.infra.invite.global` are refused. Gantry checks this before asking
the CA, so a bad name is a configuration error naming the entry.

`names` is required and lists every name. Gantry does not default to the
pod's hostname, because names under `infra.invite.global` are shared by
every project on every host: two projects with a pod called `web` would ask
for the same name. IP addresses are never included, so clients connect by
name. The
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
   its lifetime with the certificate itself, re-renders every service's files
   at once, and runs each service's `on_renew`. A failed renewal keeps the
   current certificate, warns, and retries with backoff. A certificate that
   has expired cannot renew itself, so gantry issues a new one with the
   credential, as at creation.
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

The answer carries the certificate, the chain and the root, in PEM. A
`renew` request carries the current certificate and key and gets the same
answer. The first plugin, **gantry-stepca**, signs with the Gantry credential
through the `gantry-hosts` provisioner and renews over mutual TLS with the
pod certificate. Its settings file names the CA URL, the
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

## Security Notes

- The credential key is the power to mint any `*.infra.invite.global`
  certificate, so it is root only and used only by the issuer plugin. Gantry
  never mounts it into a container.
- step-ca does not check whether an X5C credential has been revoked. A stolen
  credential keeps minting certificates until it expires (90 days now). A
  shorter credential lifetime, with Ansible or a timer renewing it more often,
  narrows that window. That is a decision for the CA's owner.
- A pod certificate cannot mint others: `gantry-hosts` refuses it as a
  credential.

## Open Questions

1. Should `on_renew` default to `restart`, which always works but costs a
   restart every 20 days, or to `none`, which is free but silently wrong for
   software that does not reload its certificates?
2. Is the full chain or the leaf alone the right default for `cert.pem`?
   Full chain suits servers. Some clients want the leaf alone.
3. Does any service need the certificate before its pod exists, for example
   a build step? This design does not cover that.
