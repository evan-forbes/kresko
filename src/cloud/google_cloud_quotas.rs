//! GCP Compute quota tracking for project `kresko-493120`.
//!
//! Constants here mirror the *granted* values from Cloud Quotas (what GCP
//! actually enforces), not the preferred values of any pending requests. When
//! a quota request is approved, bump the matching constant.
//!
//! Status as of 2026-04-15:
//! - `CPUS-ALL-REGIONS-per-project`: granted 128, **pending preferred 500**
//! - Supported regions currently show `SSD_TOTAL_GB = 500`
//!
//! Check current state:
//!   gcloud alpha quotas preferences list \
//!       --account=evan.samuel.forbes@gmail.com --project=kresko-493120 \
//!       --format="table(quotaId,quotaConfig.preferredValue,quotaConfig.grantedValue,reconciling)"
//!
//! Request more (use the personal account; the project service account does
//! not have `cloudquotas.*` permissions):
//!   gcloud alpha quotas preferences create \
//!       --account=evan.samuel.forbes@gmail.com --project=kresko-493120 \
//!       --service=compute.googleapis.com --quota-id=<ID> \
//!       --preferred-value=<N> [--dimensions=region=<REGION>] \
//!       --justification=<TEXT> --preference-id=<UNIQUE_ID>
//!
//! Note on the C3D family: `c3d-highcpu-*` has no per-family quota in Cloud
//! Quotas. It only consumes the general regional `CPUS` quota and the global
//! `CPUS-ALL-REGIONS-per-project` quota, so those two are all we track here.

use anyhow::{Result, bail};
use std::collections::HashMap;

use crate::config::{GCP_DEFAULT_DISK_SIZE_GB, Instance, Provider};

/// Project-wide CPU cap across all regions (`CPUS-ALL-REGIONS-per-project`).
pub const GCP_GLOBAL_CPU_LIMIT: u32 = 128;

/// Per-region general `CPUS` quotas (granted). Regions absent from this list
/// have no recorded quota and node assignment to them will be rejected.
pub const GCP_REGIONAL_CPU_LIMITS: &[(&str, u32)] = &[
    ("us-central1", 200),
    ("us-east1", 200),
    ("us-east4", 200),
    ("us-west1", 100),
    ("northamerica-northeast1", 200),
    ("southamerica-east1", 200),
    ("europe-west1", 200),
    ("europe-west4", 200),
    ("asia-east1", 100),
    ("asia-southeast1", 100),
    ("australia-southeast1", 100),
    ("me-west1", 100),
];

/// Per-region `SSD_TOTAL_GB` quotas (granted) for `pd-ssd` boot disks.
pub const GCP_REGIONAL_SSD_TOTAL_GB_LIMITS: &[(&str, u32)] = &[
    ("us-central1", 500),
    ("us-east1", 500),
    ("us-east4", 500),
    ("europe-west1", 500),
    ("asia-east1", 500),
    ("asia-southeast1", 500),
];

/// vCPUs consumed per machine slug. Extend when introducing a new slug.
pub fn vcpus_for_slug(slug: &str) -> Option<u32> {
    match slug {
        "c3d-highcpu-8" => Some(8),
        "c3d-highcpu-4" => Some(4),
        _ => None,
    }
}

/// Verify a planned set of GCP miners fits under the recorded regional/global
/// CPU quotas and the regional `pd-ssd` storage quotas. Call before
/// persisting a new instance list.
pub fn validate_assignment(miners: &[Instance]) -> Result<()> {
    let regional_cpu: HashMap<&str, u32> = GCP_REGIONAL_CPU_LIMITS.iter().copied().collect();
    let regional_ssd: HashMap<&str, u32> =
        GCP_REGIONAL_SSD_TOTAL_GB_LIMITS.iter().copied().collect();

    let mut per_region_cpu: HashMap<&str, u32> = HashMap::new();
    let mut per_region_ssd_gb: HashMap<&str, u32> = HashMap::new();
    let mut total: u32 = 0;

    for inst in miners {
        if inst.provider != Provider::GoogleCloud {
            continue;
        }
        let cpus = vcpus_for_slug(&inst.slug).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown vCPU count for GCP slug '{}' on instance '{}' — add it to vcpus_for_slug()",
                inst.slug,
                inst.name,
            )
        })?;
        *per_region_cpu.entry(inst.region.as_str()).or_default() += cpus;
        *per_region_ssd_gb.entry(inst.region.as_str()).or_default() +=
            GCP_DEFAULT_DISK_SIZE_GB as u32;
        total += cpus;
    }

    for (region, used) in &per_region_cpu {
        let Some(&limit) = regional_cpu.get(region) else {
            bail!(
                "GCP region '{region}' has no recorded CPU quota \
                 (planned {used} vCPU). Add it to GCP_REGIONAL_CPU_LIMITS \
                 or remove instances in this region.",
            );
        };
        if *used > limit {
            bail!("GCP region '{region}' would use {used} vCPU but granted CPUS quota is {limit}",);
        }
    }

    for (region, used) in &per_region_ssd_gb {
        let Some(&limit) = regional_ssd.get(region) else {
            bail!(
                "GCP region '{region}' has no recorded SSD quota \
                 (planned {used} GB pd-ssd). Add it to \
                 GCP_REGIONAL_SSD_TOTAL_GB_LIMITS or remove instances in this region.",
            );
        };
        if *used > limit {
            bail!(
                "GCP region '{region}' would use {used} GB of pd-ssd boot disks \
                 but granted SSD_TOTAL_GB quota is {limit}. Reduce the node count, \
                 shrink GCP_DEFAULT_DISK_SIZE_GB, or raise the regional SSD quota.",
            );
        }
    }

    if total > GCP_GLOBAL_CPU_LIMIT {
        bail!(
            "GCP plan uses {total} vCPU across all regions but the granted \
             global CPUS-ALL-REGIONS limit is {GCP_GLOBAL_CPU_LIMIT}. Reduce \
             the node count or wait for the pending quota increase to be granted.",
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_assignment;
    use crate::config::{Instance, NodeType, Provider};

    fn gcp_instance(region: &str, slug: &str, idx: usize) -> Instance {
        Instance::new_base(
            NodeType::Miner,
            Provider::GoogleCloud,
            slug,
            region,
            &format!("miner-{idx}-{region}"),
            "test-experiment",
            "full",
        )
    }

    #[test]
    fn accepts_assignment_within_ssd_quota() {
        let miners: Vec<_> = (0..12)
            .map(|idx| gcp_instance("us-east4", "c3d-highcpu-4", idx))
            .collect();

        validate_assignment(&miners).expect("12 * 40 GB should fit in 500 GB SSD quota");
    }

    #[test]
    fn rejects_assignment_exceeding_ssd_quota() {
        let miners: Vec<_> = (0..13)
            .map(|idx| gcp_instance("us-east4", "c3d-highcpu-4", idx))
            .collect();

        let error =
            validate_assignment(&miners).expect_err("13 * 40 GB should exceed 500 GB SSD quota");
        let message = error.to_string();

        assert!(message.contains("SSD_TOTAL_GB"));
        assert!(message.contains("520 GB"));
        assert!(message.contains("500"));
    }
}
