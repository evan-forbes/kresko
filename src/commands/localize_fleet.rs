//! Render local-fleet node configs from a generated genesis payload.
//!
//! The mempool-load harness runs N zakurad nodes on one host, each on its own
//! `127.0.0.x` loopback. Kresko's `genesis` renders one-node-per-host configs
//! (0.0.0.0 binds, a shared `/root` state dir), so each has to be re-pointed at
//! its own address and directories before the fleet can come up. This used to
//! be ~350 lines of TOML rewriting in `mempool-load-lab.py::prepare_node_dirs`;
//! it now lives in Rust next to the config generation it depends on.
//!
//! The rewrite is byte-identical to the former Python output: `zebra_config`
//! keeps it line-oriented rather than round-tripping through `toml::Value`.

use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::zebra_config::{self, LocalFleetNode};

/// Kresko's fixed per-host P2P port; the fleet varies the bind address instead.
const P2P_PORT: u16 = 18233;

/// For each miner in `config.json`, read its generated `payload/<name>` config
/// and write a localized `nodes/<name>/zakura.toml` (plus the bootstrap variant
/// and a copy of the funded key) bound to that node's own loopback and dirs.
///
/// `directory` must be the same resolved lab directory the harness runs against,
/// so the absolute node-relative paths baked into the config match at runtime.
pub fn run(directory: &str, miner_nodes: usize) -> Result<()> {
    let dir = Path::new(directory);
    let config = Config::load(dir)?;
    config.require_local_genesis("localize-fleet")?;

    let node_count = config.miners.len();
    if node_count == 0 {
        anyhow::bail!("No miners configured in config.json; run `kresko genesis` first.");
    }
    if miner_nodes < 1 || miner_nodes > node_count {
        anyhow::bail!("--miner-nodes must be between 1 and the node count ({node_count})");
    }

    let payload = dir.join("payload");
    let local_genesis = payload.join("local_genesis");
    if !local_genesis.is_dir() {
        anyhow::bail!(
            "missing {}; run `kresko genesis` before `localize-fleet`",
            local_genesis.display()
        );
    }
    let checkpoints_path = local_genesis.join("checkpoints.txt").display().to_string();

    // Every node's loopback address, in the same order `genesis` funded them, so
    // each node's peer list is every other node's address.
    let node_ips: Vec<String> = config.miners.iter().map(|i| i.public_ip.clone()).collect();

    for (index, inst) in config.miners.iter().enumerate() {
        let name = inst.parsed_hostname();
        let node_dir = dir.join("nodes").join(&name);
        for sub in ["state", "identity", "cookie"] {
            std::fs::create_dir_all(node_dir.join(sub))
                .with_context(|| format!("creating {}/{sub}", node_dir.display()))?;
        }

        let peers: Vec<String> = node_ips
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, ip)| format!("{ip}:{P2P_PORT}"))
            .collect();

        let node = LocalFleetNode {
            ip: inst.public_ip.clone(),
            node_dir: node_dir.clone(),
            is_miner: index < miner_nodes,
            peers,
        };

        for (src_name, dst_name, bootstrap) in [
            ("zebrad.toml", "zakura.toml", false),
            ("zebrad.bootstrap.toml", "zakura.bootstrap.toml", true),
        ] {
            let src_path = payload.join(&name).join(src_name);
            let text = std::fs::read_to_string(&src_path)
                .with_context(|| format!("reading generated config {}", src_path.display()))?;
            let localized = zebra_config::localize_local_fleet_config(
                &text,
                &node,
                &checkpoints_path,
                bootstrap,
            )
            .with_context(|| format!("localizing {src_name} for node {name}"))?;
            std::fs::write(node_dir.join(dst_name), localized)
                .with_context(|| format!("writing {}/{dst_name}", node_dir.display()))?;
        }

        // The funded key travels with the node so the blast can spend its premine.
        let funded_src = payload.join(&name).join("funded_key.json");
        std::fs::copy(&funded_src, node_dir.join("funded_key.json"))
            .with_context(|| format!("copying {}", funded_src.display()))?;

        println!(
            "  {} -> nodes/{name}/zakura.toml ({})",
            inst.name,
            if index < miner_nodes {
                "miner"
            } else {
                "relay"
            }
        );
    }

    println!(
        "Localized {node_count} node config(s) under {}",
        dir.join("nodes").display()
    );
    Ok(())
}
