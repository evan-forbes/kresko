from __future__ import annotations

from pathlib import Path


SCRIPT_PATH = Path("scripts/node_init.sh")
PUBLIC_SCRIPT_PATH = Path("scripts/node_init_public.sh")


def _script_text() -> str:
    return SCRIPT_PATH.read_text(encoding="utf-8")


def _public_script_text() -> str:
    return PUBLIC_SCRIPT_PATH.read_text(encoding="utf-8")


def test_node_init_no_longer_munges_zebrad_toml_with_sed_or_awk():
    """The Rust `kresko config` subcommand replaced the awk/sed surgery
    that used to live in node_init.sh. Make sure no zebrad.toml mutations
    sneak back in via shell string surgery."""
    text = _script_text()
    # Allow these awk uses: hostname parsing, sha256sum, manifest reader.
    forbidden_substrings = [
        "/^[[:space:]]*genesis_block_path",
        "/^[[:space:]]*post_blossom_pow_target_spacing",
        "/^[[:space:]]*miner_address",
        "/^[[:space:]]*genesis_hash",
        "/^[[:space:]]*initial_testnet_peers",
        "/^[[:space:]]*initial_mainnet_peers",
        "prepare_bootstrap_config",
    ]
    for needle in forbidden_substrings:
        assert needle not in text, (
            f"node_init.sh still contains string surgery {needle!r}; "
            f"convert it to a `kresko config` call"
        )


def test_node_init_uses_kresko_config_for_toml_mutations():
    """Each TOML mutation must go through the TOML-aware `kresko config` CLI."""
    text = _script_text()
    assert "kresko config strip-genesis-block-path" in text
    assert "kresko config get-miner-address" in text
    assert "kresko config set-miner-address" in text
    assert "kresko config get-genesis-hash" in text


def test_node_init_uses_prerendered_bootstrap_config_when_present():
    """Payload now ships a pre-rendered zebrad.bootstrap.toml. node_init.sh
    must prefer it over re-rendering, falling back only on legacy payloads."""
    text = _script_text()
    assert "payload/$parsed_hostname/zebrad.bootstrap.toml" in text
    # Fallback path for older payloads still exists, via TOML-aware render.
    assert "kresko config render-bootstrap" in text


def test_node_init_writes_kresko_env_and_unified_log_tree():
    """All logs land under /root/logs/* and the kresko env is dumped to a
    file that tmux_start_command sources. Without this, sessions started
    over the wire miss KRESKO_RPC_URL/PORT and tools default to bogus
    localhost ports."""
    text = _script_text()
    assert "/root/.kresko/env" in text, "node_init must write the kresko env file"
    assert "KRESKO_RPC_URL=$KRESKO_RPC_URL" in text
    assert "KRESKO_RPC_PORT=$KRESKO_RPC_PORT" in text
    # Bootstrap and miner logs both land under /root/logs/*.
    assert "/root/logs/bootstrap.log" in text
    assert "/root/logs/miner.log" in text
    assert "/root/logs/zebrad.log" in text
    # Legacy log paths must not regress.
    assert "/root/logs.bootstrap" not in text
    assert "/root/kresko-mine.log" not in text


def test_node_init_detects_early_zebrad_exit_and_dumps_log_tail():
    """If zebrad dies in the first ~10s, the deploy must exit non-zero with
    the log tail rather than dropping into bash silently."""
    text = _script_text()
    # zebrad runs in the background so the wrapper can poll for early exit.
    assert "zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a \"$LOG_FILE\" &" in text
    assert "kill -0 \"$zebrad_pid\"" in text
    # The tail-and-exit path must run before `exec bash`.
    assert "Tail of $LOG_FILE" in text
    assert "exited within 10s with code" in text


def test_node_init_serializes_seed_block_commits():
    text = _script_text()

    assert "for retry in $(seq 1 30)" in text
    assert "retry $retry/30" in text
    assert "for attempt in $(seq 1 60)" in text
    assert 'if [ "$current_height" -ge "$submitted" ]' in text
    assert "Timed out waiting for seed block $submitted to commit" in text


def test_public_node_init_runs_zebrad_under_systemd_with_raised_fd_limit():
    text = _public_script_text()

    assert 'mkdir -p /root/logs /root/traces' in text
    assert 'payload_root="/root/kresko/payload"' in text
    assert 'source "${payload_root}/vars.sh"' in text
    # zebrad runs as a supervised systemd service (crash-restart +
    # reboot-persistence), not a bare backgrounded process.
    assert '/etc/systemd/system/zebrad.service' in text
    assert 'Restart=on-failure' in text
    # Raised open-file ceiling: rocksdb SST files + peer sockets otherwise
    # exhaust the OS default of 1024 and panic the state service with EMFILE.
    assert 'LimitNOFILE=1048576' in text
    assert 'systemctl enable zebrad' in text
    # The old export crashed this zebrad (unknown config field node_id); it
    # must be gone now that the unit runs with a clean env.
    assert 'export ZEBRA_NODE_ID="$parsed_hostname"' not in text
    # Tracing env vars are carried into the unit instead, so download-traces
    # keeps working.
    assert '_kresko_unit_env+="Environment=' in text
    assert 'mkdir -p "$(dirname "$_kresko_var_value")"' in text
    assert 'mkdir -p "$(dirname "$_kresko_var_value")/traces"' not in text
    assert "falling back to P2P block sync" in text


def test_public_node_init_requires_zakura_and_explicit_legacy_setting():
    text = _public_script_text()

    assert 'Zakura P2P enablement' in text
    assert 'legacy Zebra P2P setting' in text
    assert '(true|false)' in text
    assert 'stable Zakura node identity' in text
    assert 'Zakura P2P listen address' in text
    assert 'Zakura bootstrap peers' in text
