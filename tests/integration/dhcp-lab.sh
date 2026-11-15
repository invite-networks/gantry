#!/bin/sh
# An isolated DHCP lab: a host bridge with no uplink, and a dnsmasq server
# in its own namespace attached to it. Pods use the bridge as their
# macvlan parent, so DHCP traffic never reaches a real network.
#   tests/integration/dhcp-lab.sh up|down
set -eu
BR=gtdhcp0
NS=gtdhcpsrv
case "${1:-}" in
  up)
    ip link add "$BR" type bridge 2>/dev/null || true
    ip link set "$BR" up
    ip netns add "$NS" 2>/dev/null || true
    ip link add gtdhcpv0 type veth peer name gtdhcpv1 2>/dev/null || true
    ip link set gtdhcpv0 master "$BR" up
    ip link set gtdhcpv1 netns "$NS" 2>/dev/null || true
    ip -n "$NS" link set lo up
    ip -n "$NS" link set gtdhcpv1 up
    ip -n "$NS" addr add 10.99.0.1/24 dev gtdhcpv1 2>/dev/null || true
    ip netns exec "$NS" dnsmasq --no-daemon --conf-file=/dev/null --interface=gtdhcpv1 --bind-interfaces \
      --dhcp-range=10.99.0.100,10.99.0.150,255.255.255.0,2m --dhcp-option=option:router,10.99.0.1 \
      --dhcp-option=option:dns-server,10.99.0.1 --domain=lab.test --port=53 --address=/lab.test/10.99.0.1 \
      --dhcp-leasefile=/run/gtdhcp.leases --pid-file=/run/gtdhcp.pid >/run/gtdhcp.log 2>&1 &
    sleep 1
    echo "dhcp lab up: bridge $BR, server 10.99.0.1, pool 10.99.0.100-150"
    ;;
  down)
    [ -f /run/gtdhcp.pid ] && kill "$(cat /run/gtdhcp.pid)" 2>/dev/null || true
    ip netns del "$NS" 2>/dev/null || true
    ip link del gtdhcpv0 2>/dev/null || true
    ip link del "$BR" 2>/dev/null || true
    rm -f /run/gtdhcp.pid /run/gtdhcp.leases
    echo "dhcp lab down"
    ;;
  *) echo "usage: $0 up|down" >&2; exit 2 ;;
esac
