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
networks:
  lan: { driver: macvlan, parent: eth0, ipam: { config: [ { subnet: 192.0.2.0/24, gateway: 192.0.2.1 } ] } }
pods:
  web: { networks: { lan: { ipv4_address: 192.0.2.10 } } }
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
ln -s /tmp /etc/gantry/link; out=$($S link config 2>&1) && bad "symlinked project allowed" || ok "symlinked project: $out"

# Direct sudo for anything else must fail.
su -s /bin/sh gantry -c 'sudo -n /usr/bin/gantry version' >/dev/null 2>&1 && bad "sudo gantry directly" || ok "no direct sudo gantry"
su -s /bin/sh gantry -c 'sudo -n id' >/dev/null 2>&1 && bad "sudo id" || ok "no sudo id"

echo "--- logs"; grep -h "gantry-run\|gantry-ssh" /var/log/auth.log 2>/dev/null | tail -3

echo "--- purge"
apt-get purge -y -qq gantry >/dev/null 2>&1
getent passwd gantry && bad "user left after purge" || ok "user removed"
ls /etc/sudoers.d/gantry /etc/ssh/sshd_config.d/gantry.conf /etc/gantry/remote 2>&1 | grep -c "No such" | grep -q 3 && ok "files removed" || bad "files left: $(ls /etc/gantry /etc/sudoers.d /etc/ssh/sshd_config.d)"
echo "RESULT $pass passed, $fail failed"
