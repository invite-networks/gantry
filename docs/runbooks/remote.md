# Remote control over SSH

| Field | Value |
|---|---|
| Owner | Host operator (root) |
| Applies to | Gantry 0.1.x installed from the .deb |
| Validation | Package install, the restricted account, every refusal and purge tested in a Debian 13 container (`tests/packaging/remote.sh`, 68 checks; the rename from the old `gantry` account in `tests/packaging/rename.sh`); sshd scoping also checked on Debian 12 and Ubuntu 22.04. Live run on the Debian 13 reference host (2026-11-16): Docker, Podman and kiri in one pod, every command in section 3, the refusals in sections 3 and 4, and key revocation. |

Gantry has to run on the host that runs the containers, as root. To drive
it from another machine, the package sets up a `gantry-remote` account that can log
in with SSH keys and do one thing: operate projects that root has put in
an installed project directory (see below). It cannot change those projects, get a shell, forward
ports or run anything else.

```
your machine                    host
ssh gantry-remote@host demo up   -->   sshd (Match User gantry-remote)
                                  ForceCommand gantry-ssh
                                    sudo gantry-run demo up
                                      cd /etc/gantry/demo && gantry up -d
```

## 1. What the package installs

| Path | Purpose |
|---|---|
| account `gantry-remote` | System account, home `/var/lib/gantry` (root owned), no password. Created on install, removed on purge. |
| `/etc/ssh/sshd_config.d/gantry.conf` | For the `gantry-remote` account only: keys come from `/etc/gantry/remote/authorized_keys`, key login only, the forced command, no TTY, no forwarding. |
| `/etc/gantry/remote/authorized_keys` | Root owned and empty at install, so nobody can log in until you add a key. |
| `/etc/sudoers.d/gantry` | `gantry-remote` may run `/usr/libexec/gantry/gantry-run` as root, and nothing else. The same file lets people in the `gantry` group run gantry as root (see [install](install.md#8-run-gantry-without-sudo)). |
| `/usr/libexec/gantry/gantry-ssh` | The forced command. Splits the request into words without a shell and calls `gantry-run` through sudo. |
| `/usr/libexec/gantry/gantry-run` | Checks the project and the command, then runs Gantry as root in the project directory. |

Sources: `contrib/remote/`. The package recommends `sudo` and
`openssh-server`. Without them the account exists but cannot be used.

Before 0.2.0 the account was named `gantry`. Upgrading renames it to
`gantry-remote` in place (same uid, same keys file) and gives it its own
group, so callers change `ssh gantry@host` to `ssh gantry-remote@host`. If
the account was logged in during the upgrade, the rename is skipped with a
warning; run `sudo dpkg-reconfigure gantry` once its sessions end.

## 2. Enable it on a host

Trigger: a host should be operated remotely.

1. Put the project in place as root, in a directory listed in
   `/etc/gantry/project-dirs`, the same way boot finds it
   (see [install](install.md#6-run-a-project-at-boot)):

   ```sh
   sudo install -d -m 0755 /mnt/storage/myproject
   sudo install -m 0644 gantry.yaml /mnt/storage/myproject/
   ```

2. Add one key per caller (a person, CI, an automation host), with a
   comment that says who it is and, ideally, where it may connect from:

   ```sh
   echo 'from="10.0.0.0/24" ssh-ed25519 AAAA... ci@invite' |
     sudo tee -a /etc/gantry/remote/authorized_keys
   ```

   The file is read on every login, so no reload is needed.

3. If sshd restricts logins with `AllowUsers` or `AllowGroups`, add
   `gantry-remote` (user or group) there and reload ssh.

Validate from the caller:

```sh
ssh gantry-remote@host version
ssh gantry-remote@host myproject config --services
```

## 3. Use it

```sh
ssh gantry-remote@host ls                               # projects with pods on the host
ssh gantry-remote@host myproject up --wait              # always detached
ssh gantry-remote@host myproject ps
ssh gantry-remote@host myproject logs --tail 100 web
ssh gantry-remote@host myproject logs -f web            # Ctrl-C to stop following
ssh gantry-remote@host myproject restart web
ssh gantry-remote@host myproject pull
ssh gantry-remote@host myproject down
ssh gantry-remote@host myproject pod ls
ssh gantry-remote@host myproject firewall show            # every pod, with hit counts
ssh gantry-remote@host gantry myproject ps              # a leading "gantry" is ignored
```

| Command | Allowed flags and arguments |
|---|---|
| `ls`, `version` | none (no project) |
| `up` | `--wait`, `--wait-timeout N`, `--force-recreate`, `--build`, `--pull always\|missing\|never`, services. Runs with `-d` always. |
| `down` | `-t N` |
| `ps` | none |
| `config` | `--services`, `--pods`, `--json` |
| `logs` | `-f`, `-t`, `--tail N\|all`, services |
| `start`, `pull`, `build`, `top` | services |
| `stop`, `restart` | `-t N`, services |
| `pod ls`, `pod inspect POD`, `firewall show [POD] [--raw]` | as shown |

Not available remotely, because each one either destroys data or runs
arbitrary commands as root: `down -v`, `exec`, `run`, `kill`, `rm`,
`pod exec`, `firewall reload`, `setup`, `data move`, and the `-f`, `-p`,
`--project-directory`, `--env-file`, `--profile` and `--runtime` flags. Use
an admin login for those.

Requests may only contain letters, digits, spaces and `. _ = -`. Anything
else is refused before a shell or sudo is involved.

## 4. What is checked before every command

`gantry-run` refuses to run when:

* the project name is not a plain name, or it is not found exactly once in
  the directories listed in `/etc/gantry/project-dirs` (such as
  `/etc/gantry/<name>` or `/mnt/<storage>/<name>`; symlinks are not
  followed);
* `/etc/gantry/project-dirs`, or any directory above the project, is not
  owned by root or is writable by a group or others;
* `/etc/gantry` or the project directory is not owned by root, or is
  writable by a group or others;
* a file directly in the project directory (the Compose file, `.env`) is
  not owned by root or is writable by a group or others;
* anything under the project directory is owned by the `gantry-remote` account, or
  is writable by others.

Data directories owned by container users (for example `./data` owned by
uid 999 for a database) are fine, though data is better kept outside the
project directory (see [install](install.md#6-run-a-project-at-boot)). A project the `gantry-remote` account could
change is not.

## 5. Security notes

* **Treat the keys as operator credentials.** They cannot change what runs,
  but they can stop and start every project on the host and read its logs.
* **Changing a project is a separate, privileged step.** Copy project files
  in through your provisioning path, as root. Never give the `gantry-remote`
  account write access to `/etc/gantry`.
* **Do not add `gantry-remote` to the `docker` or `gantry` group** or give it other sudo
  rules. Either one is a root path around `gantry-run`.
* **`config` hides likely secrets.** Environment values and build
  arguments whose names look secret (`*_KEY`, `*PASSWORD*`, `*TOKEN*` and
  similar), and passwords inside URLs, print as `<redacted>`. `logs` still
  shows whatever the containers print.
* **Audit.** sshd logs which key logged in (its fingerprint). `gantry-ssh`
  logs the source address and request, and `gantry-run` and sudo log the
  command run, all to the auth log (`journalctl -t gantry-run -t gantry-ssh -t sudo`).

## 6. Revoke access or disable it

* One caller: delete their line from `/etc/gantry/remote/authorized_keys`.
* Keys are checked only at login. A connection that is already open (a
  `logs -f`, or a client multiplexing with `ControlMaster`) keeps working
  until it closes. To cut them off now: `sudo pkill -u gantry-remote`.
* Every caller on a host: empty the file (`sudo truncate -s 0 /etc/gantry/remote/authorized_keys`).
  The account stays but cannot log in.
* Remove everything: `sudo apt purge gantry` removes the account, the key
  file and both drop-ins. `apt remove` keeps the drop-ins and the keys, as
  it does for any configuration.

## 7. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `Permission denied (publickey)` | Key not in `/etc/gantry/remote/authorized_keys`, a `from=` that does not match, or `AllowUsers`/`AllowGroups` excludes `gantry-remote`. Check `journalctl -u ssh`. |
| `sudo: a password is required` | `/etc/sudoers.d/gantry` is missing or does not validate: `sudo visudo -cf /etc/sudoers.d/gantry`. |
| `gantry-run: config: no installed project ...` | No directory of that name in the directories listed in `/etc/gantry/project-dirs`. |
| `... is installed more than once ...` | The same name exists in two project directories. Keep one. |
| `... (above the project) is not owned by root ...` | The project sits under a directory others can change. On NFS this is usually a squashed export: root's files show up as `nobody`. |
| `... is not owned by root or is writable by a group or others` | Fix the path named: `sudo chown root:root <path>` and `sudo chmod go-w <path>`. Do not `chown -R` the project, which would break container data directories. |
| `... is not available remotely` | By design; see section 3. |
| `PTY allocation request failed` | Harmless. The account never gets a TTY; the command still runs. |
| `ls` shows a name that `ssh gantry-remote@host <name> ...` rejects | `ls` prints the project's `name:` from the Compose file; remote commands take the directory name under `/etc/gantry`. Keep the two the same. |
