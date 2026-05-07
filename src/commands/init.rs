use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const ENV_STUB: &str = "# Kresko credentials. Uncomment and fill in for the providers you use.
# DIGITALOCEAN_TOKEN=
# AWS_ACCESS_KEY_ID=
# AWS_SECRET_ACCESS_KEY=
# KRESKO_S3_BUCKET=
# KRESKO_SSH_KEY_NAME=
# KRESKO_SSH_KEY_PATH=~/.ssh/id_ed25519
";

const CONFIG_STUB: &str = "# Kresko global defaults. Per-experiment settings live in
# ~/.kresko/experiments/<name>/run.py.
";

const SOURCE_EXPERIMENTS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/experiments");

pub fn run(name: Option<String>, from: String, force: bool, source: Option<String>) -> Result<()> {
    let home = kresko_home()?;
    ensure_home(&home)?;
    write_stub_if_missing(&home.join(".env"), ENV_STUB)?;
    write_stub_if_missing(&home.join("config.toml"), CONFIG_STUB)?;

    let source_root = source
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SOURCE_EXPERIMENTS_DIR));

    match name {
        None => seed_bundled_experiments(&home, &source_root)?,
        Some(name) => scaffold(&home, &source_root, &name, &from, force)?,
    }
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
    for sub in ["experiments", "runs", "assets", "cache"] {
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

fn seed_bundled_experiments(home: &Path, source_root: &Path) -> Result<()> {
    if !source_root.is_dir() {
        eprintln!(
            "warning: bundled experiments dir {} not found; skipping",
            source_root.display(),
        );
        return Ok(());
    }
    let dest_root = home.join("experiments");
    let mut copied = 0usize;
    let mut skipped = 0usize;
    let mut entries: Vec<_> = fs::read_dir(source_root)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let dest = dest_root.join(&name);
        if dest.exists() {
            println!("skipped {} (exists)", name.to_string_lossy());
            skipped += 1;
            continue;
        }
        copy_dir_recursive(&entry.path(), &dest)?;
        println!("copied {}", name.to_string_lossy());
        copied += 1;
    }
    println!("done: {copied} copied, {skipped} skipped");
    Ok(())
}

fn scaffold(
    home: &Path,
    source_root: &Path,
    name: &str,
    from: &str,
    force: bool,
) -> Result<()> {
    validate_slug(name)?;
    let source = source_root.join(from);
    if !source.is_dir() {
        bail!(
            "reference experiment {:?} not found at {}",
            from,
            source.display(),
        );
    }
    let dest = home.join("experiments").join(name);
    if dest.exists() {
        if !force {
            bail!(
                "{} already exists; pass --force to overwrite",
                dest.display(),
            );
        }
        fs::remove_dir_all(&dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    copy_dir_recursive(&source, &dest)?;
    println!("created {}", dest.display());
    Ok(())
}

fn validate_slug(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        bail!("experiment name must not be empty");
    }
    let first_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let rest_ok = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_');
    if !first_ok || !rest_ok {
        bail!(
            "invalid experiment name {:?}: must start with [a-z0-9] and contain only [a-z0-9_-]",
            value,
        );
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &dst_path)
                .with_context(|| format!("copying to {}", dst_path.display()))?;
        }
    }
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

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn make_source(label: &str) -> PathBuf {
        let root = unique_tmp_dir(&format!("src-{label}"));
        write(
            &root.join("pyinfra_do_smoke/run.py"),
            "print('hi from smoke')\n",
        );
        write(
            &root.join("pyinfra_do_smoke/payload/data.txt"),
            "smoke payload\n",
        );
        write(&root.join("other_exp/run.py"), "print('other')\n");
        root
    }

    #[test]
    fn validate_slug_accepts_underscored_names() {
        validate_slug("pyinfra_do_smoke").unwrap();
        validate_slug("my-exp").unwrap();
        validate_slug("a").unwrap();
        validate_slug("0a").unwrap();
    }

    #[test]
    fn validate_slug_rejects_bad_names() {
        assert!(validate_slug("").is_err());
        assert!(validate_slug("-foo").is_err());
        assert!(validate_slug("Foo").is_err());
        assert!(validate_slug("foo bar").is_err());
        assert!(validate_slug("foo/bar").is_err());
    }

    #[test]
    fn ensure_home_creates_subdirs_and_stubs() {
        let home = unique_tmp_dir("home");
        ensure_home(&home).unwrap();
        for sub in ["experiments", "runs", "assets", "cache"] {
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
    fn seed_copies_then_skips() {
        let home = unique_tmp_dir("seed-home");
        ensure_home(&home).unwrap();
        let source = make_source("seed");

        seed_bundled_experiments(&home, &source).unwrap();
        assert!(home.join("experiments/pyinfra_do_smoke/run.py").is_file());
        assert!(
            home.join("experiments/pyinfra_do_smoke/payload/data.txt")
                .is_file()
        );
        assert!(home.join("experiments/other_exp/run.py").is_file());

        // Mutate to verify second run doesn't overwrite.
        fs::write(home.join("experiments/pyinfra_do_smoke/run.py"), "EDITED\n").unwrap();
        seed_bundled_experiments(&home, &source).unwrap();
        assert_eq!(
            fs::read_to_string(home.join("experiments/pyinfra_do_smoke/run.py")).unwrap(),
            "EDITED\n",
        );
    }

    #[test]
    fn scaffold_refuses_existing_without_force() {
        let home = unique_tmp_dir("scaffold-noforce");
        ensure_home(&home).unwrap();
        let source = make_source("scaffold-noforce");
        let target = home.join("experiments/my-exp");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("untouched.txt"), "keep me\n").unwrap();

        let err = scaffold(&home, &source, "my-exp", "pyinfra_do_smoke", false).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err}"
        );
        assert!(target.join("untouched.txt").is_file());
    }

    #[test]
    fn scaffold_force_overwrites() {
        let home = unique_tmp_dir("scaffold-force");
        ensure_home(&home).unwrap();
        let source = make_source("scaffold-force");
        let target = home.join("experiments/my-exp");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("untouched.txt"), "should be wiped\n").unwrap();

        scaffold(&home, &source, "my-exp", "pyinfra_do_smoke", true).unwrap();
        assert!(target.join("run.py").is_file());
        assert!(!target.join("untouched.txt").exists());
    }

    #[test]
    fn scaffold_with_explicit_from() {
        let home = unique_tmp_dir("scaffold-from");
        ensure_home(&home).unwrap();
        let source = make_source("scaffold-from");

        scaffold(&home, &source, "alt", "other_exp", false).unwrap();
        assert_eq!(
            fs::read_to_string(home.join("experiments/alt/run.py")).unwrap(),
            "print('other')\n",
        );
    }

    #[test]
    fn scaffold_unknown_reference_errors() {
        let home = unique_tmp_dir("scaffold-missing-ref");
        ensure_home(&home).unwrap();
        let source = make_source("scaffold-missing-ref");

        let err = scaffold(&home, &source, "x", "no-such-ref", false).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
