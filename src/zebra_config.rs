use anyhow::Result;

use crate::config::Instance;

/// Default zebrad.toml template.
pub const DEFAULT_ZEBRAD_TOML: &str = r#"[consensus]
checkpoint_sync = true

[mempool]
eviction_memory_time = "1h"
tx_cost_limit = 80000000

[mining]
miner_address = ""

[network]
network = "Testnet"
listen_addr = "0.0.0.0:18233"
initial_testnet_peers = []

[state]
cache_dir = "/root/.cache/zebra"

[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false

[tracing]
use_color = false
"#;

/// Generate a per-node zebrad.toml with the correct peer list.
///
/// Takes the template content as a string, replaces the `initial_testnet_peers`
/// line with the actual peer IPs (excluding the current node).
pub fn generate_node_config(
    template: &str,
    current_node: &Instance,
    all_instances: &[Instance],
) -> Result<String> {
    let peers: Vec<String> = all_instances
        .iter()
        .filter(|inst| inst.name != current_node.name)
        .filter(|inst| inst.public_ip != "TBD")
        .map(|inst| format!("\"{}:18233\"", inst.public_ip))
        .collect();

    let peer_list = format!("[{}]", peers.join(", "));

    // Replace the initial_testnet_peers line
    let mut result = String::new();
    for line in template.lines() {
        if line.trim().starts_with("initial_testnet_peers") {
            result.push_str(&format!("initial_testnet_peers = {peer_list}"));
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    Ok(result)
}
