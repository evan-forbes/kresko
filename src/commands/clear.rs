use anyhow::Result;
use futures::stream::{self, StreamExt};

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;

pub async fn run_traces(nodes: &str, workers: usize, directory: &str) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.miners, nodes)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    if targets.is_empty() {
        println!("No matching nodes found.");
        return Ok(());
    }

    println!(
        "Clearing remote trace files on {} node(s) matching '{nodes}'...",
        targets.len()
    );

    let command = format!(
        "bash -lc {}",
        shell_single_quote(&build_trace_clear_script())
    );
    let mut failures = Vec::new();
    let mut results = stream::iter(targets.into_iter().map(|inst| {
        let ip = inst.public_ip;
        let name = inst.name;
        let key = key.clone();
        let command = command.clone();

        async move {
            let result = ssh::ssh_exec(&ip, &key, &command).await;
            (name, ip, result)
        }
    }))
    .buffer_unordered(workers);

    while let Some((name, ip, result)) = results.next().await {
        match result {
            Ok(output) => {
                let output = output.trim();
                if output.is_empty() {
                    println!("  {name}: no trace files found");
                } else {
                    println!("  {name}: {output}");
                }
            }
            Err(err) => {
                eprintln!("  {name}: failed to clear traces: {err}");
                failures.push(format!("{name} ({ip}): {err}"));
            }
        }
    }

    if !failures.is_empty() {
        anyhow::bail!(
            "failed to clear traces on {} node(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }

    println!("Remote trace clear complete.");
    Ok(())
}

fn build_trace_clear_script() -> String {
    r#"set -eo pipefail
if [ -f /root/payload/vars.sh ]; then
    # shellcheck disable=SC1091
    . /root/payload/vars.sh
fi
set -u

declare -a candidate_dirs=()
for var_name in ${!ZEBRA@}; do
    case "$var_name" in
        ZEBRA_*TRACE*_DIR|ZEBRA_*TRACING*_DIR)
            dir="${!var_name}"
            if [ -n "$dir" ]; then
                candidate_dirs+=("$dir")
            fi
            ;;
        ZEBRA_*TRACE*_FILE|ZEBRA_*TRACING*_FILE)
            file="${!var_name}"
            if [ -n "$file" ]; then
                candidate_dirs+=("$(dirname "$file")/traces")
            fi
            ;;
    esac
done
if [ -n "${KRESKO_TRACE_DIR:-}" ]; then
    candidate_dirs+=("$KRESKO_TRACE_DIR")
fi
candidate_dirs+=("/root/.cache/zebra/traces")
candidate_dirs+=("/root/.cache/kresko/txblast-traces")
candidate_dirs+=("/root/traces")

seen_dirs="$(mktemp /tmp/kresko-clear-traces-seen.XXXXXX)"
trap 'rm -f "$seen_dirs"' EXIT

cleared_files=0
cleared_dirs=0
for dir in "${candidate_dirs[@]}"; do
    [ -d "$dir" ] || continue
    real_dir="$(readlink -f "$dir" 2>/dev/null || printf '%s' "$dir")"
    if grep -Fxq "$real_dir" "$seen_dirs"; then
        continue
    fi
    printf '%s\n' "$real_dir" >> "$seen_dirs"

    count="$(find "$dir" -maxdepth 1 \( -type f -o -type l \) -printf . | wc -c)"
    if [ "$count" -eq 0 ]; then
        continue
    fi
    find "$dir" -maxdepth 1 \( -type f -o -type l \) -delete
    cleared_files=$((cleared_files + count))
    cleared_dirs=$((cleared_dirs + 1))
done

if [ "$cleared_files" -gt 0 ]; then
    printf 'deleted %s trace file(s) from %s director%s\n' "$cleared_files" "$cleared_dirs" "$([ "$cleared_dirs" -eq 1 ] && printf y || printf ies)"
fi
"#
    .to_owned()
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::build_trace_clear_script;

    #[test]
    fn trace_clear_script_removes_files_from_candidate_trace_directories() {
        let script = build_trace_clear_script();
        assert!(script.contains("ZEBRA_*TRACE*_DIR|ZEBRA_*TRACING*_DIR"));
        assert!(script.contains("/root/.cache/zebra/traces"));
        assert!(script.contains("/root/.cache/kresko/txblast-traces"));
        assert!(script.contains("/root/traces"));
        assert!(script.contains(r#"\( -type f -o -type l \) -delete"#));
        assert!(!script.contains("rm -rf"));
    }
}
