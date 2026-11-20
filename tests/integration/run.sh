#!/bin/sh
# Live end to end tests. Needs root, a built gantry on PATH, the engines
# under test, and a LAN segment with three free addresses:
#
#   sudo GT_PARENT=vlan4 GT_PREFIX=29 GT_GATEWAY=192.0.2.222 \
#        GT_IP1=192.0.2.217 GT_IP2=192.0.2.218 GT_IP3=192.0.2.219 \
#        tests/integration/run.sh [docker] [kiri] [dhcp] [dhcpmove] [ipv6] [heal]
#
# With no arguments it runs docker, dhcp and heal. The dhcp scenario uses
# an isolated bridge (tests/integration/dhcp-lab.sh) and needs dnsmasq. To
# lease from a real network's DHCP server instead, set GT_DHCP_PARENT to
# the parent interface and GT_DHCP_GATEWAY to the gateway the lease hands
# out (the lease release check is skipped, as the server log is not ours).
# dhcpmove moves the lab's DHCP pool under a running pod (about a minute).
# heal runs the health watcher on Docker and kiri (GT_HEAL_RUNTIMES) with
# loopback-only pods, so it needs no addresses (about a minute each).
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
  check "shared hostname" sh -c "$K exec -T client hostname | grep -qx webhost && $K exec -T server hostname | grep -qx webhost"
  for ns in uts ipc net; do
    check "shared $ns namespace" sh -c "[ \"\$($K exec -T client readlink /proc/self/ns/$ns)\" = \"\$($K exec -T server readlink /proc/self/ns/$ns)\" ]"
  done
  check "loopback between services in a pod" sh -c "$K exec -T client sh -c 'nc -w 3 127.0.0.1 8080' | grep -q hello-from-web"
  check "service name resolves to loopback" sh -c "$K exec -T client ping -c1 -W1 server | grep -q '127.0.0.1'"
  check "other pod reachable by service name" sh -c "$K exec -T client nc -z -w 3 redis 6379"
  check_not "firewall blocks other -> data:6379" sh -c "$K exec -T probe nc -z -w 3 redis 6379"
  check "firewall allows other -> web:8080" sh -c "$K exec -T probe sh -c 'nc -w 3 server 8080' | grep -q hello-from-web"
  check_not "firewall blocks data -> web:8080" sh -c "$K exec -T redis sh -c 'nc -z -w 3 server 8080'"
  check "default route via gateway" sh -c "$K pod exec web ip route | grep -q 'default via $GT_GATEWAY'"
  check "nft table loaded in pod" sh -c "$K pod exec data nft list table inet gantry"
  check "pod without firewall: section gets the defaults" sh -c "$K pod exec other nft list table inet gantry | grep -q 'default: reject local subnet'"
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
  K="gantry -f $here/dhcp/gantry.yaml"
  if [ -n "${GT_DHCP_PARENT:-}" ]; then
    # A real network with its own DHCP server.
    : "${GT_DHCP_GATEWAY:?set GT_DHCP_GATEWAY with GT_DHCP_PARENT}"
    export GT_DHCP_PARENT
    gw=$GT_DHCP_GATEWAY
    in_pool="inet [0-9.]*/[0-9]*"
    dns="^nameserver "
  else
    command -v dnsmasq >/dev/null || { bad "dnsmasq not installed"; return; }
    had_lab=0
    ip link show gtdhcp0 >/dev/null 2>&1 && had_lab=1
    "$here/dhcp-lab.sh" up >/dev/null
    gw=10.99.0.1
    in_pool="inet 10.99.0.1[0-5][0-9]/24"
    dns="nameserver 10.99.0.1"
  fi
  $K down >/dev/null 2>&1
  if $K up -d >/tmp/gt-dhcp-up.log 2>&1; then ok "up with dhcp"; else bad "up with dhcp"; tail -5 /tmp/gt-dhcp-up.log; fi
  check "leased address in pool" sh -c "$K exec -T shell ip -4 addr show eth0 | grep -q '$in_pool'"
  check "default route from lease" sh -c "$K exec -T shell ip route | grep -q 'default via $gw '"
  check "resolv.conf from lease" sh -c "$K exec -T shell cat /etc/resolv.conf | grep -q '$dns'"
  check "server reachable" sh -c "$K exec -T shell ping -c1 -W2 $gw"
  if [ -d /run/systemd/system ]; then
    # Start the pod from a service that ends as soon as `up` returns, the
    # way restarting gantry-agent would. The agent must live in its own
    # scope and keep running after systemd cleans up the service's cgroup.
    $K down >/dev/null 2>&1
    systemd-run --quiet --wait --collect --unit=gt-dhcp-starter \
      --setenv=PATH="$PATH" --setenv=GT_DHCP_PARENT="${GT_DHCP_PARENT:-}" \
      --setenv=GANTRY_RUNTIME="${GANTRY_RUNTIME:-docker}" \
      $K up -d >/dev/null 2>&1
    sleep 1
    pid=$(pgrep -f "dhcp-agent.*dhcplab" | head -1)
    check "agent outlives the service that started it" test -n "$pid"
    check "agent runs in its own scope" sh -c "grep -q 'gantry-dhcp-.*\.scope' /proc/$pid/cgroup"
    check "address kept after the service ended" sh -c "$K exec -T shell ip -4 addr show eth0 | grep -q '$in_pool'"
  fi
  $K down >/dev/null 2>&1
  check_not "agent stopped" pgrep -f "dhcp-agent.*dhcplab"
  [ -n "${GT_DHCP_PARENT:-}" ] && return
  check "lease released on down" grep -q DHCPRELEASE /run/gtdhcp.log
  [ "$had_lab" = 1 ] || "$here/dhcp-lab.sh" down >/dev/null
}

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
lease_addr() {
  gantry -f "$here/dhcpmove/gantry.yaml" pod inspect client 2>/dev/null |
    sed -n 's/.*"address": "\([0-9.]*\)".*/\1/p' | head -1
}

dhcpmove() {
  echo "== dhcp address change"
  "$here/dhcp-lab.sh" down >/dev/null
  "$here/dhcp-lab.sh" up >/dev/null
  K="gantry -f $here/dhcpmove/gantry.yaml"
  $K down >/dev/null 2>&1
  if ! $K up -d >/tmp/gt-dhcpmove-up.log 2>&1; then
    bad "up"; tail -5 /tmp/gt-dhcpmove-up.log; return
  fi
  a=$(lease_addr)
  check "first lease $a in the first pool" sh -c "echo $a | grep -q '^10.99.0.1[0-5][0-9]$'"
  "$here/dhcp-lab.sh" move >/dev/null
  b=$a
  for _ in $(seq 1 40); do
    b=$(lease_addr)
    [ -n "$b" ] && [ "$b" != "$a" ] && break
    sleep 3
  done
  check "new lease $b from the moved pool" sh -c "echo $b | grep -q '^10.99.0.16[0-9]$'"
  sleep 2
  check "watcher hosts file follows" grep -q "^$b[[:space:]]client" /run/gantry/dhcpmove/pods/watcher/hosts
  check "watcher firewall allows the new address" sh -c "$K pod exec watcher nft list ruleset | grep -q '$b'"
  check_not "watcher firewall dropped the old address" sh -c "$K pod exec watcher nft list ruleset | grep -q '$a'"
  $K down >/dev/null 2>&1
  "$here/dhcp-lab.sh" down >/dev/null
  "$here/dhcp-lab.sh" up >/dev/null
}

ipv6() {
  echo "== ipv6"
  ip link show gtdhcp0 >/dev/null 2>&1 || "$here/dhcp-lab.sh" up >/dev/null
  K="gantry -f $here/ipv6/gantry.yaml"
  $K down >/dev/null 2>&1
  if ! $K up -d >/tmp/gt-ipv6-up.log 2>&1; then
    bad "up"; tail -5 /tmp/gt-ipv6-up.log; return
  fi
  check "pod a has fd99::31" sh -c "$K pod exec a ip -6 addr show eth0 | grep -q 'fd99::31/64'"
  check "hosts file has both families" sh -c "grep -q '^fd99::31[[:space:]]a' /run/gantry/v6/pods/b/hosts && grep -q '^10.99.0.31[[:space:]]a' /run/gantry/v6/pods/b/hosts"
  check "ping6 by pod name" sh -c "$K exec -T sb ping -6 -c1 -W2 a </dev/null"
  check "firewall allows b over IPv6" sh -c "$K exec -T sb nc -w3 fd99::31 7000 </dev/null | grep -q v6-ok"
  check_not "firewall blocks c over IPv6" sh -c "$K exec -T sc nc -w3 fd99::31 7000 </dev/null | grep -q v6-ok"
  $K down >/dev/null 2>&1
}

heal() {
  # The packaged watcher would hold the host lock; pause it for the test.
  unit_was_active=
  if systemctl is-active --quiet gantry-supervise.service 2>/dev/null; then
    unit_was_active=1
    systemctl stop gantry-supervise.service
  fi
  for rt in ${GT_HEAL_RUNTIMES:-docker kiri}; do
    echo "== heal on $rt"
    export GANTRY_RUNTIME=$rt
    K="gantry -f $here/heal/gantry.yaml"
    $K down >/dev/null 2>&1
    if ! $K up -d >/tmp/gt-heal-up.log 2>&1; then
      bad "up ($rt)"; tail -5 /tmp/gt-heal-up.log; continue
    fi
    sleep 8 # two failed checks at 2s each, then marked unhealthy
    timeout -s TERM 25 gantry supervise >/tmp/gt-heal.log 2>&1 &
    sup=$!
    sleep 3
    check "a second watcher is refused" sh -c "gantry supervise 2>&1 | grep -q 'another health watcher'"
    wait "$sup"
    check "restart: sick was restarted" grep -q "gtheal/sick: Restarted" /tmp/gt-heal.log
    check "restart: sick is running" sh -c "$K ps | grep sick | grep -q running"
    check "stop: doomed was stopped" grep -q "gtheal/doomed: Stopped" /tmp/gt-heal.log
    check_not "stop: doomed stays down" sh -c "$K ps | grep doomed | grep -q running"
    check_not "none: calm untouched" grep -q "gtheal/calm" /tmp/gt-heal.log
    check "watcher stopped cleanly" grep -q "health watcher stopped" /tmp/gt-heal.log
    $K down >/dev/null 2>&1
  done
  unset GANTRY_RUNTIME
  [ -n "$unit_was_active" ] && systemctl start gantry-supervise.service
}

targets=${*:-docker dhcp heal}
for t in $targets; do
  case $t in
    docker|kiri) e2e "$t" ;;
    heal) heal ;;
    dhcp) dhcp ;;
    dhcpmove) dhcpmove ;;
    ipv6) ipv6 ;;
    *) echo "unknown scenario $t" >&2; exit 2 ;;
  esac
done
echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
