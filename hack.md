 Target only asia-pacific-0:

  TARGET_IP=168.144.173.250
  SESSION=snapshot-sync-latest-20260621T035925Z
  LOG=/root/logs/snapshot-sync-latest-20260621T035925Z.log
  ARCHIVE=zebra-mainnet-20260620T070754Z-3384241.tar.zst
  EXPECTED_HEIGHT=3384241

  1. Check whether the detached run is still active:

  ssh root@$TARGET_IP 'hostname; tmux ls 2>/dev/null || true; tail -n 180 /root/logs/snapshot-sync-latest-20260621T035925Z.log'

  If tmux still shows snapshot-sync-latest-20260621T035925Z, do not restart anything. Wait until it exits. The running script should download, checksum-
  verify, extract, remove the archive, run /root/kresko/payload/node_init.sh, and start zebrad.

  2. Confirm snapshot extraction succeeded:

  ssh root@$TARGET_IP '
  grep -n "Snapshot extracted; zebrad will resume from the snapshot height" /root/logs/snapshot-sync-latest-20260621T035925Z.log
  ! grep -n "Snapshot hydration failed" /root/logs/snapshot-sync-latest-20260621T035925Z.log
  du -sh /root/.cache/zebra
  '

  3. Verify service and Zakura-only config:

  ssh root@$TARGET_IP '
  systemctl show zebrad -p ActiveState -p SubState -p NRestarts --no-pager
  grep -nE "legacy_p2p|v2_p2p|replace_legacy_syncer|initial_mainnet_peers" /root/.config/zebrad.toml
  ss -ltnp | grep -E ":(8232|8233|8234)[[:space:]]" || true
  ss -lunp | grep -E ":(8232|8233|8234|8235)[[:space:]]" || true
  '

  Expected:

  - ActiveState=active
  - SubState=running
  - legacy_p2p = false
  - v2_p2p = true
  - TCP 8232 present
  - TCP 8233 absent
  - UDP 8234 present

  4. Verify RPC height is at or above the new snapshot height 3384241:

  ssh root@$TARGET_IP '
  curl -sS --max-time 8 \
    --data-binary "{\"jsonrpc\":\"1.0\",\"id\":\"verify\",\"method\":\"getblockchaininfo\",\"params\":[]}" \
    -H "content-type: text/plain;" \
    http://127.0.0.1:8232/
  '

  5. If the tmux session exited but the log does not contain the success line, inspect before changing anything:

  ssh root@$TARGET_IP '
  tail -n 240 /root/logs/snapshot-sync-latest-20260621T035925Z.log
  ls -lh /tmp/zebra-mainnet-20260620T070754Z-3384241.tar.zst*
  ps -ef | grep -E "[a]ria2c|[z]std|[t]ar|[z]ebrad" || true
  '

  Only resume manually if the archive is incomplete or the script clearly failed. Use the same 2026-06-20 archive/checksum, not the older 2026-06-12 one.

