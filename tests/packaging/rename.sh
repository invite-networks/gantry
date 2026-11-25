#!/bin/sh
# Package test for the upgrade from the pre-0.2.0 remote control account
# (gantry, in its own gantry group) to gantry-remote. Creates the old
# account the way older packages did, installs the .deb, and checks the
# rename. Run it like remote.sh, in a throwaway container:
#   docker run --rm -v "$PWD/target/debian:/out:ro" \
#     -v "$PWD/tests/packaging/rename.sh:/test.sh:ro" debian:trixie sh /test.sh
set -u
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq openssh-server sudo nftables >/dev/null 2>&1

pass=0 fail=0
ok() { pass=$((pass + 1)); echo "PASS $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }

useradd --system --user-group --home-dir /var/lib/gantry --shell /bin/sh \
	--comment "Gantry remote control" gantry
usermod -p '*' gantry
old_uid=$(id -u gantry)
useradd -m -G gantry carol # a person someone put in the old group

apt-get install -y -qq /out/*.deb 2>&1 | grep -i gantry

getent passwd gantry && bad "old account still there" || ok "old account renamed"
[ "$(id -u gantry-remote 2>/dev/null)" = "$old_uid" ] && ok "gantry-remote kept uid $old_uid" || bad "uid changed: $(id gantry-remote 2>&1)"
[ "$(id -gn gantry-remote)" = gantry-remote ] && ok "primary group gantry-remote" || bad "primary group $(id -gn gantry-remote)"
id -nG gantry-remote | tr ' ' '\n' | grep -qx gantry && bad "gantry-remote still in gantry" || ok "gantry-remote out of the gantry group"
su -s /bin/sh gantry-remote -c 'sudo -n /usr/bin/gantry version' >/dev/null 2>&1 && bad "gantry-remote may sudo gantry" || ok "gantry-remote cannot sudo gantry"
su -s /bin/sh gantry-remote -c 'sudo -n -l /usr/libexec/gantry/gantry-run' >/dev/null 2>&1 && ok "gantry-remote may still run gantry-run" || bad "gantry-run lost"
su carol -c 'sudo -n /usr/bin/gantry version' >/dev/null 2>&1 && ok "people in gantry may sudo gantry" || bad "carol cannot sudo gantry"
dpkg-reconfigure gantry >/dev/null 2>&1
[ "$(id -u gantry-remote)" = "$old_uid" ] && ! getent passwd gantry >/dev/null && ok "reconfigure is a no-op" || bad "reconfigure changed accounts"
echo "RESULT $pass passed, $fail failed"
