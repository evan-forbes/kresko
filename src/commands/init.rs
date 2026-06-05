use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const ENV_STUB: &str = "# Kresko credentials. Uncomment and fill in for the providers you use.
# DIGITALOCEAN_TOKEN=
# VULTR_API_KEY=
# AWS_ACCESS_KEY_ID=
# AWS_SECRET_ACCESS_KEY=
# AWS_S3_BUCKET=
# KRESKO_SSH_KEY_NAME=
# KRESKO_SSH_KEY_PATH=~/.ssh/id_ed25519
";

const CONFIG_STUB: &str = "# Kresko global defaults. Fleets are defined in plain Python scripts
# using the `kresko` package (from kresko import Fleet); fleet state lives
# under ~/.kresko/fleets/<name>/.
#
# Tunables are read from the environment (set them here or in .env):
#   KRESKO_STATE_SNAPSHOT_URL=http://mainnet.zebra.legends.sh/   # default snapshot mirror
#   KRESKO_RPC_PORT=8232                                         # default RPC port for `status`
";

/// Initialize the ~/.kresko/ home: fleets/, assets/, cache/ plus credential
/// and config stubs. There are no bundled templates to copy — a fleet is just
/// a Python script that imports `kresko.Fleet`.
pub fn run() -> Result<()> {
    let home = kresko_home()?;
    ensure_home(&home)?;
    write_stub_if_missing(&home.join(".env"), ENV_STUB)?;
    write_stub_if_missing(&home.join("config.toml"), CONFIG_STUB)?;
    println!("initialized {}", home.display());
    Ok(())
}

fn kresko_home() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("KRESKO_HOME") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home).join(".kresko"))
}

fn ensure_home(home: &Path) -> Result<()> {
    for sub in ["fleets", "assets", "cache"] {
        let dir = home.join(sub);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(())
}

fn write_stub_if_missing(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut f = fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kresko-init-{label}-{}-{nanos}-{n}",
            std::process::id(),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_home_creates_fleet_subdirs_and_stubs() {
        let home = unique_tmp_dir("home");
        ensure_home(&home).unwrap();
        for sub in ["fleets", "assets", "cache"] {
            assert!(home.join(sub).is_dir(), "{sub} not created");
        }
        write_stub_if_missing(&home.join(".env"), ENV_STUB).unwrap();
        write_stub_if_missing(&home.join("config.toml"), CONFIG_STUB).unwrap();
        assert!(home.join(".env").is_file());
        assert!(home.join("config.toml").is_file());
        assert_eq!(fs::read_to_string(home.join(".env")).unwrap(), ENV_STUB);
    }

    #[test]
    fn write_stub_if_missing_does_not_overwrite() {
        let home = unique_tmp_dir("stub");
        ensure_home(&home).unwrap();
        let env = home.join(".env");
        fs::write(&env, "MARKER=keep-me\n").unwrap();
        write_stub_if_missing(&env, ENV_STUB).unwrap();
        assert_eq!(fs::read_to_string(&env).unwrap(), "MARKER=keep-me\n");
    }

    #[test]
    fn does_not_create_legacy_experiment_dirs() {
        let home = unique_tmp_dir("legacy");
        ensure_home(&home).unwrap();
        assert!(!home.join("experiments").exists());
        assert!(!home.join("runs").exists());
    }
}
