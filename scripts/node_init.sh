#!/bin/bash
set -euo pipefail

ARCHIVE_NAME="payload.tar.gz"
export DEBIAN_FRONTEND=noninteractive

echo "=== Installing dependencies ==="
apt-get update -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold"
apt-get install -y build-essential ufw curl jq chrony tmux btop nethogs

echo "=== Configuring firewall ==="
ufw allow 18233/tcp   # P2P
ufw allow 18232/tcp   # RPC
ufw allow 18233/udp
ufw allow 18232/udp
ufw --force enable || true

echo "=== Configuring time sync ==="
systemctl enable chrony
systemctl start chrony

echo "=== Configuring BBR congestion control ==="
modprobe tcp_bbr || true
sysctl -w net.core.default_qdisc=fq
sysctl -w net.ipv4.tcp_congestion_control=bbr
echo "net.core.default_qdisc=fq" >> /etc/sysctl.conf
echo "net.ipv4.tcp_congestion_control=bbr" >> /etc/sysctl.conf

echo "=== Extracting payload ==="
tar -xzf /root/$ARCHIVE_NAME -C /root/
source /root/payload/vars.sh

cd $HOME
hostname=$(hostname)
parsed_hostname=$(echo $hostname | awk -F'-' '{print $1 "-" $2}')

echo "=== Installing binaries ==="
cp payload/build/zebrad /usr/local/bin/zebrad
chmod +x /usr/local/bin/zebrad

if [ -f payload/build/kresko ]; then
    cp payload/build/kresko /usr/local/bin/kresko
    chmod +x /usr/local/bin/kresko
fi

echo "=== Setting up zebra config ==="
mkdir -p /root/.cache/zebra
mkdir -p /root/.config
cp payload/$parsed_hostname/zebrad.toml /root/.config/zebrad.toml

echo "=== Node: $parsed_hostname ==="
echo "=== Starting zebrad ==="

LOG_FILE="/root/logs"
zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a "$LOG_FILE"
