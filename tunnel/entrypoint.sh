#!/bin/sh
set -e

# Start redsocks (transparent SOCKS5 redirector)
redsocks -c /etc/redsocks.conf
sleep 1

# iptables: redirect all outbound TCP through redsocks
# Exclude: SSH target (avoid loop), loopback, private networks
iptables -t nat -N REDSOCKS
iptables -t nat -A REDSOCKS -d ${SSH_HOST}/32 -j RETURN
iptables -t nat -A REDSOCKS -d 127.0.0.0/8 -j RETURN
iptables -t nat -A REDSOCKS -d 10.0.0.0/8 -j RETURN
iptables -t nat -A REDSOCKS -d 172.16.0.0/12 -j RETURN
iptables -t nat -A REDSOCKS -d 192.168.0.0/16 -j RETURN
iptables -t nat -A REDSOCKS -p tcp -j REDIRECT --to-ports 12345
iptables -t nat -A OUTPUT -p tcp -j REDSOCKS

# Start SSH SOCKS tunnel with auto-reconnect
exec autossh -M 0 \
    -N -D 0.0.0.0:1080 \
    -o StrictHostKeyChecking=no \
    -o ServerAliveInterval=30 \
    -o ServerAliveCountMax=3 \
    -o ExitOnForwardFailure=yes \
    -i /root/.ssh/tunnel_key \
    -p ${SSH_PORT:-22} \
    ${SSH_USER:-root}@${SSH_HOST}
