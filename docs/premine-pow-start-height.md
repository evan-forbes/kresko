# Premine via PoW-disabled bootstrap + `pow_start_height`

## Goal

Every premine block header's timestamp is close to wall-clock-now, with
consecutive spacings equal to `target_spacing_secs`. When PoW turns on at
the seeded tip (via zebra's `pow_start_height`) the first live-mined block
has no multi-thousand-second gap.

## Background

Today kresko seeds experiments with a cached chain of genesis + premine +
maturity-padding blocks, Equihash-mined against the live network's
`pow_limit`. Because mining is slow, the chain is cached. But block hashes
commit to the header including `time`, so cached timestamps can't be
rewritten in place without re-hashing the whole chain.

When the cache is reused hours or days later, the seeded tip's timestamp is
stale. The first live-mined block's header time jumps to wall-clock-now,
producing one block with ~5275 s spacing and all others exactly at
`target_spacing_secs` — the observed artifact.

Fix: generate the bootstrap with `disable_pow: true`, anchored to
`SystemTime::now()`, and have consensus enforce PoW from the seeded tip via
`pow_start_height`.

## Current status — zebra

**No zebra changes needed.** The full mechanism is already in place:

- `zebra-chain/src/local_genesis.rs:237-241` — when the generator is called
  with `disable_pow: true`, `pow_start_height` is automatically set to
  `Height(blocks.len() as u32)` and threaded into the returned `Network`.
- `zebra-chain/src/local_genesis.rs` — `seeded_tip_time: None` anchors
  generation to `SystemTime::now()`; block N gets time
  `now - (tail_len - N) * target_spacing_secs`, giving exact target spacing.
- `zebra-consensus/src/block.rs:211` — semantic verifier skips Equihash when
  `network.should_skip_pow_at_height(height)` is true.
- `zebra-consensus/src/checkpoint.rs:601` — checkpoint verifier honors the
  same gate.
- Premine blocks themselves are cheap: `Transaction::V1` coinbases only, no
  shielded proofs, no chain-history tree. Measured: 203 blocks in 0.01 s
  (zebra's `generated_chain_can_include_maturity_padding_blocks` test);
  256 blocks ≈13 ms.

**Constraint if any zebra change turns out to be necessary:** it must be
generic — no kresko-specific names, comments, flags, or behavior. The
`local_genesis` facility is general-purpose; keep it that way.

## Current status — kresko

Implemented (no-cache variant).

- `src/premine.rs` — rewritten. `generate(&CalibrationSignature)` returns an
  in-memory `PremineBundle` (`disable_pow: true`, `seeded_tip_time: None`,
  `MATURITY_PADDING_BLOCKS` unchanged). Cache layer fully removed: no
  `resolve_premine`, `try_load_by_key`, `cache_key`, `cache_dir`,
  `DEFAULT_CACHE_KEY`, `default_cache_root`, `default_solver_threads`,
  `PremineBundle::load_from_dir`, `read_text_file`, `ResolveOutcome`.
  `PremineManifest` gains `pow_start_height: Option<u32>`; payload files are
  emitted via `PremineBundle::payload_files()`.
- `src/commands/genesis.rs` — `prepare_premine_local_genesis` now calibrates,
  signs, and calls `premine::generate(&sig)` inline every run. No cache
  branch, no warning path, no `--premine-cache-key`. `report_calibration`
  now prints the generation wallclock instead of hit/miss.
- `src/commands/premine.rs` — deleted. The subcommand was a cache-warmup
  helper; meaningless without a cache.
- `src/main.rs` — `Premine` subcommand and `--premine-cache-key` flag
  removed.
- `src/config.rs` — `LocalGenesisConfig.premine_cache_key` field removed.
- `src/zebra_config.rs` — `LocalTestnetParameters` gains
  `pow_start_height: Option<u32>`; `apply_local_testnet_parameters` emits
  `pow_start_height = N` inside `[network.testnet_parameters]` when set.
  `prepare_premine_local_genesis` threads `manifest.pow_start_height` into
  it; `prepare_generated_local_genesis` passes `None`.

## Verification

- `cargo test --package kresko` passes (69 tests).
  - New test `premine::tests::generated_bundle_has_uniform_spacing_anchored_to_now`
    asserts `observed_min_spacing_secs == observed_max_spacing_secs ==
    target_spacing_secs`, `disable_pow` is true, `pow_start_height ==
    seeded_block_count + 1`, and `seeded_tip_time ∈ [before, after]` of the
    call.
  - Updated `zebra_config::tests::injects_local_testnet_parameters` asserts
    the TOML contains `pow_start_height = 257`.
- `cargo check` clean on kresko.
- End-to-end (not done as part of this change): run a small PoW experiment,
  inspect the first ~20 live-mined block headers, confirm spacing is tight
  around `target_spacing_secs` with no multi-thousand-second outlier at the
  seeded→live boundary.

## Out of scope

- Kresko-specific additions to zebra. If a zebra helper turns out to be
  genuinely needed, it must be named and documented generically.
- Changing `FUNDED_KEY_COUNT` or `MATURITY_PADDING_BLOCKS`.
- Shielded-tx workload behavior; premine is transparent-only V1 coinbases
  by design.
- Keys-only cache variant (option 2a). Not implemented; generation is fast
  enough that per-call regeneration is simpler.
