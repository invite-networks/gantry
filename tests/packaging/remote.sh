#!/bin/sh
# Package test for remote control over SSH. Installs the .deb in a throwaway
# Debian container and drives the restricted gantry account over ssh:
#   docker run --rm -v "$PWD/target/debian:/out:ro" \
#     -v "$PWD/tests/packaging/remote.sh:/test.sh:ro" debian:trixie sh /test.sh
# Do not run it on a real host: it installs packages, starts sshd and purges gantry.
set -u
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq openssh-server openssh-client sudo nftables >/dev/null 2>&1
mkdir -p /run/sshd
ssh-keygen -A >/dev/null
apt-get install -y -qq /out/*.deb 2>&1 | grep -i gantry
/usr/sbin/sshd

pass=0 fail=0
ok() { pass=$((pass + 1)); echo "PASS $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }

echo "--- install state"
getent passwd gantry
stat -c '%U %a %n' /etc/gantry /etc/gantry/remote /etc/gantry/remote/authorized_keys /etc/sudoers.d/gantry /usr/libexec/gantry/gantry-run /var/lib/gantry
visudo -cqf /etc/sudoers.d/gantry && ok sudoers valid || bad sudoers
# The Match block must not leak to other users.
sshd -T -C user=root,host=x,addr=127.0.0.1 | grep -qi '^forcecommand none' && ok "no ForceCommand for root" || bad "ForceCommand leaks to root"
# Main-config lines after the Include (Debian sets X11Forwarding yes there) still apply to everyone.
grep -n "^Include\|^X11Forwarding" /etc/ssh/sshd_config
sshd -T -C user=root,host=x,addr=127.0.0.1 | grep -qi '^x11forwarding yes' && ok "main config after Include still applies" || bad "Match block swallowed the main config"
sshd -T -C user=root,host=x,addr=127.0.0.1 | grep -qi '^permittty yes' && ok "root keeps PermitTTY" || bad "PermitTTY leaks"
sshd -T -C user=gantry,host=x,addr=127.0.0.1 | grep -i '^forcecommand /usr/libexec/gantry/gantry-ssh' >/dev/null && ok "ForceCommand for gantry" || bad "no ForceCommand for gantry"

mkdir -p /etc/gantry/demo
cat > /etc/gantry/demo/gantry.yaml <<'Y'
name: demo
pods:
  web: { network: [ { parent: eth0, address: 192.0.2.10/24 } ] }
services:
  nginx: { image: nginx:alpine, pod: web }
Y
ssh-keygen -q -t ed25519 -N '' -f /root/k
S="ssh -i /root/k -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR gantry@127.0.0.1"

$S demo config --services >/dev/null 2>&1 && bad "login before a key was added" || ok "no login without a key"
cat /root/k.pub >> /etc/gantry/remote/authorized_keys

out=$($S demo config --services 2>&1) && [ "$out" = nginx ] && ok "config --services: $out" || bad "config: $out"
out=$($S gantry demo config --pods 2>&1) && [ "$out" = web ] && ok "gantry prefix: $out" || bad "prefix: $out"
out=$($S version 2>&1); echo "$out" | grep -q '^gantry' && ok "version" || bad "version: $out"
out=$($S ls 2>&1); [ $? -eq 0 ] && ok "ls" || bad "ls: $out"

# Each of these must be refused.
for c in "demo exec nginx sh" "demo run nginx sh" "demo down -v" "demo pod exec web sh" \
	"demo firewall reload" "demo kill" "demo up --pull sometimes" "demo logs --tail x" \
	"../etc config" "demo -f /tmp/x.yaml up" "nope config" "demo config --services --json; id" \
	'demo config $(id)' "demo up -f x" "demo up --project-directory=/tmp" "demo logs -- -f" \
	"demo pod inspect ../x" "demo stop -t 1x" "setup docker"; do
	out=$($S "$c" 2>&1) && bad "allowed: $c -> $out" || ok "refused: $c -> $(echo "$out" | tail -1)"
done
$S -t demo config --services 2>&1 | grep -q nginx && ok "tty request still runs" || bad "tty"
ssh -i /root/k -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -N -L 9999:127.0.0.1:22 gantry@127.0.0.1 -o ExitOnForwardFailure=yes &
sleep 2; kill $! 2>/dev/null
ssh -i /root/k -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -o ConnectTimeout=3 -p 9999 root@127.0.0.1 true 2>/dev/null && bad "port forwarding works" || ok "no port forwarding"

# Ownership checks: a writable project must be refused.
chmod 775 /etc/gantry/demo; out=$($S demo config --services 2>&1) && bad "group writable dir allowed" || ok "group writable dir: $out"; chmod 755 /etc/gantry/demo
chown gantry /etc/gantry/demo/gantry.yaml; out=$($S demo config 2>&1) && bad "gantry-owned file allowed" || ok "gantry-owned file: $out"; chown root /etc/gantry/demo/gantry.yaml
mkdir -p /etc/gantry/demo/app; chown gantry /etc/gantry/demo/app; out=$($S demo config 2>&1) && bad "gantry-owned subdir allowed" || ok "gantry-owned subdir: $out"; rm -r /etc/gantry/demo/app
mkdir -p /etc/gantry/demo/data; chown 999:999 /etc/gantry/demo/data; out=$($S demo config --services 2>&1) && ok "container-owned data dir allowed" || bad "data dir refused: $out"; rm -r /etc/gantry/demo/data
ln -s /tmp /etc/gantry/link; out=$($S link config 2>&1) && bad "symlinked project allowed" || ok "symlinked project: $out"; rm /etc/gantry/link
# A project kept next to its data under /mnt, found through project-dirs.
grep -qx '/mnt/\*' /etc/gantry/project-dirs && ok "project-dirs ships /mnt/*" || bad "project-dirs: $(cat /etc/gantry/project-dirs)"
mkdir -p /mnt/store/demo2; cp /etc/gantry/demo/gantry.yaml /mnt/store/demo2/
out=$($S demo2 config --services 2>&1) && [ "$out" = nginx ] && ok "project under /mnt: $out" || bad "project under /mnt refused: $out"
chmod 775 /mnt/store; out=$($S demo2 config 2>&1) && bad "group writable parent allowed" || ok "group writable parent: $out"; chmod 755 /mnt/store
chown gantry /mnt/store; out=$($S demo2 config 2>&1) && bad "gantry-owned parent allowed" || ok "gantry-owned parent: $out"; chown root /mnt/store
chmod 777 /mnt/store/demo2; out=$($S demo2 config 2>&1) && bad "world writable project allowed" || ok "world writable project: $out"; chmod 755 /mnt/store/demo2
mkdir -p /mnt/other/demo2; out=$($S demo2 config 2>&1) && bad "duplicate name allowed" || ok "duplicate name: $out"; rmdir /mnt/other/demo2
chmod 664 /etc/gantry/project-dirs; out=$($S demo2 config 2>&1) && bad "writable project-dirs allowed" || ok "writable project-dirs: $out"; chmod 644 /etc/gantry/project-dirs

# Local project storage is created on install, root owned and not writable
# by others, and left alone when it already exists.
[ "$(stat -c '%U:%G %a' /mnt/container-local)" = "root:root 755" ] && ok "container-local created" || bad "container-local: $(stat -c '%U:%G %a' /mnt/container-local 2>&1)"
chmod 750 /mnt/container-local; dpkg-reconfigure gantry >/dev/null 2>&1
[ "$(stat -c '%a' /mnt/container-local)" = 750 ] && ok "existing container-local untouched" || bad "container-local changed"; chmod 755 /mnt/container-local

# Boot: the package ships gantry-boot.service, enables it, and turns 0.3.1's
# per-project gantry@<name> links into boot registrations.
[ -f /lib/systemd/system/gantry-boot.service ] || [ -f /usr/lib/systemd/system/gantry-boot.service ] && ok "gantry-boot.service installed" || bad "no gantry-boot.service"
[ -L /etc/systemd/system/multi-user.target.wants/gantry-boot.service ] && ok "gantry-boot.service enabled" || bad "gantry-boot.service not enabled"
mkdir -p /etc/systemd/system/multi-user.target.wants
ln -sf /lib/systemd/system/gantry@.service /etc/systemd/system/multi-user.target.wants/gantry@old1.service
dpkg-reconfigure gantry >/dev/null 2>&1
[ -f /var/lib/gantry/boot/old1 ] && [ ! -e /etc/systemd/system/multi-user.target.wants/gantry@old1.service ] && ok "gantry@old1 migrated to boot" || bad "gantry@ not migrated: $(ls /var/lib/gantry/boot /etc/systemd/system/multi-user.target.wants)"
rm -f /var/lib/gantry/boot/old1

# Direct sudo for anything else must fail.
su -s /bin/sh gantry -c 'sudo -n /usr/bin/gantry version' >/dev/null 2>&1 && bad "sudo gantry directly" || ok "no direct sudo gantry"
su -s /bin/sh gantry -c 'sudo -n id' >/dev/null 2>&1 && bad "sudo id" || ok "no sudo id"

echo "--- logs"; grep -h "gantry-run\|gantry-ssh" /var/log/auth.log 2>/dev/null | tail -3

echo "--- purge"
apt-get purge -y -qq gantry >/dev/null 2>&1
getent passwd gantry && bad "user left after purge" || ok "user removed"
[ -d /mnt/container-local ] && ok "container-local kept after purge" || bad "container-local removed by purge"
ls /etc/sudoers.d/gantry /etc/ssh/sshd_config.d/gantry.conf /etc/gantry/remote 2>&1 | grep -c "No such" | grep -q 3 && ok "files removed" || bad "files left: $(ls /etc/gantry /etc/sudoers.d /etc/ssh/sshd_config.d)"
echo "RESULT $pass passed, $fail failed"
