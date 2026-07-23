use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use orchard::{
    bundle::{BundleVersion, Flags},
    circuit::OrchardCircuitVersion,
    Anchor,
    builder::{Builder, BundleType, UnauthorizedBundle},
    circuit::ProvingKey,
    keys::{FullViewingKey, Scope, SpendingKey},
    value::NoteValue,
};
use rand_core_06::OsRng;

fn main() -> Result<()> {
    let recipients = parse_arg("--recipients")?.unwrap_or(1);
    let iterations = parse_arg("--iterations")?.unwrap_or(5);

    if recipients == 0 {
        anyhow::bail!("--recipients must be greater than 0");
    }
    if iterations == 0 {
        anyhow::bail!("--iterations must be greater than 0");
    }

    let sk: SpendingKey = Option::<SpendingKey>::from(SpendingKey::from_bytes([7; 32]))
        .context("invalid test spending key")?;
    let recipient = FullViewingKey::from(&sk).address_at(0u32, Scope::External);

    let pk_start = Instant::now();
    // NU6.2 fixed the Orchard circuit, and NU6.3 added the disableCrossAddress
    // constraint. Benchmark the circuit the current network actually proves.
    let pk = ProvingKey::build(OrchardCircuitVersion::PostNu6_3);
    println!("proving_key_build_ms={}", elapsed_ms(pk_start.elapsed()));

    let mut timings = Vec::with_capacity(iterations);
    for idx in 0..iterations {
        let proving_ms = prove_output_bundle(recipients, recipient, &pk)?;
        timings.push(proving_ms);

        println!(
            "iteration={} recipients={} proving_ms={}",
            idx + 1,
            recipients,
            proving_ms,
        );
    }

    timings.sort_unstable();
    println!(
        "summary recipients={} iterations={} min_ms={} p50_ms={} max_ms={}",
        recipients,
        iterations,
        timings[0],
        timings[timings.len() / 2],
        timings[timings.len() - 1],
    );

    Ok(())
}

fn prove_output_bundle(
    recipients: usize,
    recipient: orchard::Address,
    pk: &ProvingKey,
) -> Result<u128> {
    // orchard_v3 is the Orchard pool at NU6.3; ironwood_v3() would benchmark
    // the Ironwood pool instead.
    let mut builder = Builder::new(
        BundleType::DEFAULT,
        BundleVersion::orchard_v3(),
        Flags::ENABLED,
        Option::<Anchor>::from(Anchor::from_bytes([0; 32])).context("invalid test anchor")?,
    )
    .context("failed to construct Orchard builder")?;

    for _ in 0..recipients {
        builder
            .add_output(None, recipient, NoteValue::from_raw(10), [0; 512])
            .context("failed to add Orchard output")?;
    }

    let bundle: UnauthorizedBundle<i64> = builder
        .build(OsRng)
        .context("failed to build Orchard bundle")?
        .context("expected non-empty Orchard bundle")?
        .0;

    let prove_start = Instant::now();
    bundle
        .create_proof(pk, OsRng)
        .context("failed to create Orchard proof")?;

    Ok(elapsed_ms(prove_start.elapsed()))
}

fn parse_arg(name: &str) -> Result<Option<usize>> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            let value = args
                .next()
                .with_context(|| format!("missing value for {name}"))?;
            return value
                .parse()
                .with_context(|| format!("invalid value for {name}: {value}"))
                .map(Some);
        }
    }
    Ok(None)
}

fn elapsed_ms(duration: Duration) -> u128 {
    duration.as_millis()
}
