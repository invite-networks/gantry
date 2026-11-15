#!/bin/sh
# Live end to end tests. Needs root, a built gantry on PATH, the engines
# under test, and a LAN segment with three free addresses:
#
#   sudo GT_PARENT=vlan4 GT_SUBNET=192.0.2.216/29 GT_GATEWAY=192.0.2.222 \
#        GT_IP1=192.0.2.217 GT_IP2=192.0.2.218 GT_IP3=192.0.2.219 \
#        tests/integration/run.sh [docker] [podman] [kiri] [dhcp]
#
# With no arguments it runs docker, podman and dhcp. The dhcp scenario uses
# an isolated bridge (tests/integration/dhcp-lab.sh) and needs dnsmasq.
set -u
here=$(cd "$(dirname "$0")" && pwd)
pass=0
fail=0
ok() { pass=$((pass + 1)); printf '  ok    %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }
check() { # check "description" command...
  desc=$1; shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}
check_not() {
  desc=$1; shift
  if "$@" >/dev/null 2>&1; then bad "$desc"; else ok "$desc"; fi
}

e2e() {
  rt=$1
  echo "== e2e on $rt"
  export GANTRY_RUNTIME=$rt
  K="gantry -f $here/e2e/gantry.yaml"
  $K down >/dev/null 2>&1
  if ! $K up -d >/tmp/gt-e2e-up.log 2>&1; then
    bad "up ($rt)"; tail -5 /tmp/gt-e2e-up.log; return
  fi
  ok "up ($rt)"
  check "second up changes nothing" sh -c "$K up -d | grep -qv Creating"
  check "pod web has $GT_IP1" sh -c "$K exec -T client ip -4 addr show eth0 | grep -q 'inet $GT_IP1/'"
  if [ "$rt" = kiri ]; then
    check "hostname set on kiri container" sh -c "$K exec -T client hostname | grep -qx webhost"
  else
    check "shared hostname" sh -c "$K exec -T client hostname | grep -qx webhost && $K exec -T server hostname | grep -qx webhost"
  fi
  check "loopback between services in a pod" sh -c "$K exec -T client sh -c 'nc -w 3 127.0.0.1 8080' | grep -q hello-from-web"
  check "service name resolves to loopback" sh -c "$K exec -T client ping -c1 -W1 server | grep -q '127.0.0.1'"
  check "other pod reachable by service name" sh -c "$K exec -T client nc -z -w 3 redis 6379"
  check_not "firewall blocks other -> data:6379" sh -c "$K exec -T probe nc -z -w 3 redis 6379"
  check "firewall allows other -> web:8080" sh -c "$K exec -T probe sh -c 'nc -w 3 server 8080' | grep -q hello-from-web"
  check_not "firewall blocks data -> web:8080" sh -c "$K exec -T redis sh -c 'nc -z -w 3 server 8080'"
  check "default route via gateway" sh -c "$K pod exec web ip route | grep -q 'default via $GT_GATEWAY'"
  check "nft table loaded in pod" sh -c "$K pod exec data nft list table inet gantry"
  check "host has no gantry table" sh -c "! nft list table inet gantry"
  check "ps shows redis healthy" sh -c "$K ps | grep redis | grep -q healthy"
  if [ "$rt" = docker ]; then
    cname=gt-e2e-client-1
    docker restart "$cname" >/dev/null 2>&1
    check "docker restart keeps the pod" sh -c "$K exec -T client ip -4 addr show eth0 | grep -q 'inet $GT_IP1/'"
  fi
  $K down >/dev/null 2>&1
  check_not "down removes netns" sh -c "ip netns list | grep -q gt-gt-e2e"
  check_not "down removes state" test -d /run/gantry/gt-e2e
}

dhcp() {
  echo "== dhcp"
  command -v dnsmasq >/dev/null || { bad "dnsmasq not installed"; return; }
  had_lab=0
  ip link show gtdhcp0 >/dev/null 2>&1 && had_lab=1
  "$here/dhcp-lab.sh" up >/dev/null
  K="gantry -f $here/dhcp/gantry.yaml"
  $K down >/dev/null 2>&1
  if $K up -d >/tmp/gt-dhcp-up.log 2>&1; then ok "up with dhcp"; else bad "up with dhcp"; tail -5 /tmp/gt-dhcp-up.log; fi
  check "leased address in pool" sh -c "$K exec -T shell ip -4 addr show eth0 | grep -q 'inet 10.99.0.1[0-5][0-9]/24'"
  check "default route from lease" sh -c "$K exec -T shell ip route | grep -q 'default via 10.99.0.1'"
  check "resolv.conf from lease" sh -c "$K exec -T shell cat /etc/resolv.conf | grep -q 'nameserver 10.99.0.1'"
  check "server reachable" sh -c "$K exec -T shell ping -c1 -W2 10.99.0.1"
  $K down >/dev/null 2>&1
  check "lease released on down" grep -q DHCPRELEASE /run/gtdhcp.log
  check_not "agent stopped" pgrep -f "dhcp-agent.*dhcplab"
  [ "$had_lab" = 1 ] || "$here/dhcp-lab.sh" down >/dev/null
}

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
targets=${*:-docker podman dhcp}
for t in $targets; do
  case $t in
    docker|podman|kiri) e2e "$t" ;;
    dhcp) dhcp ;;
    *) echo "unknown scenario $t" >&2; exit 2 ;;
  esac
done
echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
