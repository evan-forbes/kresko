from __future__ import annotations

from pathlib import Path


SCRIPT_PATH = Path("scripts/node_init.sh")


def _script_text() -> str:
    return SCRIPT_PATH.read_text(encoding="utf-8")


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
