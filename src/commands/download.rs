use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;

const TRACE_MISSING_MARKER: &str = "KRESKO_TRACE_MISSING";
const TRACE_TABLE_MISSING_MARKER: &str = "KRESKO_TRACE_TABLE_MISSING";

/// Per-node hard timeout for the trace tarball download. Long enough for big
/// archives on slow links, short enough that a wedged node fails the run
/// instead of hanging it forever.
const TRACE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How often to print which nodes are still pending while a multi-node
/// download is in flight.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
enum TraceSelection {
    All,
    Tables(Vec<TraceTable>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TraceTable {
    PeerMessage,
    TraceDropped,
    TxblastEvent,
    TxblastRegistry,
    TxblastNote,
    TxblastTraceDropped,
    ForkEvent,
    ForkSnapshot,
}

impl TraceTable {
    fn file_name(self) -> &'static str {
        match self {
            Self::PeerMessage => "peer_message.jsonl",
            Self::TraceDropped => "trace_dropped.jsonl",
            Self::TxblastEvent => "txblast_event.jsonl",
            Self::TxblastRegistry => "txblast_registry.jsonl",
            Self::TxblastNote => "txblast_note.jsonl",
            Self::TxblastTraceDropped => "txblast_trace_dropped.jsonl",
            Self::ForkEvent => "fork_event.jsonl",
            Self::ForkSnapshot => "fork_snapshot.jsonl",
        }
    }

    fn parse_list(input: &str) -> Result<TraceSelection> {
        let mut tables = Vec::new();
        for raw in input.split(',') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            if raw.eq_ignore_ascii_case("all") {
                return Ok(TraceSelection::All);
            }
            let table = match raw {
                "peer_message" | "peer-message" => Self::PeerMessage,
                "trace_dropped" | "trace-dropped" => Self::TraceDropped,
                "txblast_event" | "txblast-event" => Self::TxblastEvent,
                "txblast_registry" | "txblast-registry" => Self::TxblastRegistry,
                "txblast_note" | "txblast-note" => Self::TxblastNote,
                "txblast_trace_dropped" | "txblast-trace-dropped" => Self::TxblastTraceDropped,
                "fork_event" | "fork-event" => Self::ForkEvent,
                "fork_snapshot" | "fork-snapshot" => Self::ForkSnapshot,
                other => anyhow::bail!(
                    "unknown trace table: {other}. Use one of: all, peer_message, trace_dropped, txblast_event, txblast_registry, txblast_note, txblast_trace_dropped, fork_event, fork_snapshot"
                ),
            };
            if !tables.contains(&table) {
                tables.push(table);
            }
        }

        if tables.is_empty() {
            anyhow::bail!(
                "no trace tables selected. Use one of: all, peer_message, trace_dropped, txblast_event, txblast_registry, txblast_note, txblast_trace_dropped, fork_event, fork_snapshot"
            );
        }

        Ok(TraceSelection::Tables(tables))
    }
}

pub async fn run_logs(
    nodes: &str,
    workers: usize,
    no_compress: bool,
    directory: &str,
    data_subdir: Option<&str>,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let (targets, key, data_dir) = download_context(nodes, directory, data_subdir)?;

    if targets.is_empty() {
        println!("No matching nodes found.");
        return Ok(());
    }

    println!("Downloading logs from {} nodes...", targets.len());

    let mut results = stream::iter(targets.into_iter().map(|inst| {
        let ip = inst.public_ip;
        let name = inst.name;
        let key = key.clone();
        let node_dir = data_dir.join(&name);

        async move {
            std::fs::create_dir_all(&node_dir)?;
            download_logs(&ip, &key, &node_dir, &name, no_compress).await?;
            Ok::<_, anyhow::Error>(())
        }
    }))
    .buffer_unordered(workers);

    while let Some(result) = results.next().await {
        if let Err(e) = result {
            eprintln!("  Warning: {e}");
        }
    }

    println!("Downloads complete. Data saved to {}", data_dir.display());
    Ok(())
}

pub async fn run_traces(
    nodes: &str,
    workers: usize,
    tables: &str,
    directory: &str,
    data_subdir: Option<&str>,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let trace_selection = TraceTable::parse_list(tables)?;
    let (targets, key, data_dir) = download_context(nodes, directory, data_subdir)?;

    if targets.is_empty() {
        println!("No matching nodes found.");
        return Ok(());
    }

    match &trace_selection {
        TraceSelection::All => println!(
            "Downloading all files from discovered trace directories on {} nodes...",
            targets.len()
        ),
        TraceSelection::Tables(trace_tables) => {
            let table_names = trace_tables
                .iter()
                .map(|table| table.file_name())
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "Downloading structured traces ({table_names}) from {} nodes...",
                targets.len()
            );
        }
    }

    let total = targets.len();
    let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    let reporter_in_flight = in_flight.clone();
    let reporter = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
        // The first tick fires immediately; skip it so we don't print before
        // anything has had a chance to run.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let mut still: Vec<String> = {
                let guard = reporter_in_flight.lock().unwrap();
                guard.iter().cloned().collect()
            };
            if still.is_empty() {
                continue;
            }
            still.sort();
            println!(
                "  ...still downloading on {}/{}: {}",
                still.len(),
                total,
                still.join(", ")
            );
        }
    });

    let mut results = stream::iter(targets.into_iter().map(|inst| {
        let ip = inst.public_ip;
        let name = inst.name;
        let key = key.clone();
        let node_dir = data_dir.join(&name);
        let trace_selection = trace_selection.clone();
        let in_flight = in_flight.clone();

        async move {
            in_flight.lock().unwrap().insert(name.clone());
            let result: Result<()> = async {
                std::fs::create_dir_all(&node_dir)?;
                download_structured_traces(&ip, &key, &node_dir, &name, &trace_selection).await?;
                Ok(())
            }
            .await;
            in_flight.lock().unwrap().remove(&name);
            result.with_context(|| format!("{name} ({ip})"))
        }
    }))
    .buffer_unordered(workers);

    while let Some(result) = results.next().await {
        if let Err(e) = result {
            eprintln!("  Warning: {e:#}");
        }
    }

    reporter.abort();

    println!(
        "Trace downloads complete. Data saved to {}",
        data_dir.display()
    );
    Ok(())
}

async fn download_logs(
    ip: &str,
    key: &str,
    node_dir: &Path,
    name: &str,
    no_compress: bool,
) -> Result<()> {
    if !no_compress {
        // Compress logs on remote first.
        let _ = ssh::ssh_exec(ip, key, "xz -z /root/logs 2>/dev/null || true").await;

        let remote = "/root/logs.xz";
        let local = node_dir.join("logs.xz");
        match ssh::sftp_download(ip, key, remote, local.to_str().unwrap()).await {
            Ok(()) => println!("  {name}: downloaded logs.xz"),
            Err(_) => {
                let remote = "/root/logs";
                let local = node_dir.join("logs");
                ssh::sftp_download(ip, key, remote, local.to_str().unwrap()).await?;
                println!("  {name}: downloaded logs");
            }
        }
    } else {
        let remote = "/root/logs";
        let local = node_dir.join("logs");
        ssh::sftp_download(ip, key, remote, local.to_str().unwrap()).await?;
        println!("  {name}: downloaded logs");
    }

    download_optional_log(
        ip,
        key,
        node_dir,
        name,
        "/root/mine.log.jsonl",
        "mine.log.jsonl",
    )
    .await;
    download_optional_log(
        ip,
        key,
        node_dir,
        name,
        "/root/kresko-mine.log",
        "kresko-mine.log",
    )
    .await;

    Ok(())
}

async fn download_optional_log(
    ip: &str,
    key: &str,
    node_dir: &Path,
    name: &str,
    remote_path: &str,
    local_name: &str,
) {
    let local_path = node_dir.join(local_name);
    match ssh::sftp_download(ip, key, remote_path, local_path.to_str().unwrap()).await {
        Ok(()) => println!("  {name}: downloaded {local_name}"),
        Err(_) => {
            let _ = std::fs::remove_file(&local_path);
        }
    }
}

async fn download_structured_traces(
    ip: &str,
    key: &str,
    node_dir: &Path,
    name: &str,
    selection: &TraceSelection,
) -> Result<()> {
    let trace_archive = node_dir.join("traces.tar.gz");
    let trace_script = build_trace_download_script(selection);

    println!("  {name}: downloading structured traces...");
    match ssh::ssh_exec_to_file(
        ip,
        key,
        &trace_script,
        trace_archive.to_str().unwrap(),
        TRACE_DOWNLOAD_TIMEOUT,
    )
    .await
    {
        Ok(bytes) => {
            unpack_trace_archive(&trace_archive, &node_dir.join("traces"))?;
            let _ = std::fs::remove_file(&trace_archive);
            println!(
                "  {name}: downloaded structured traces ({:.1} MiB compressed)",
                bytes as f64 / (1024.0 * 1024.0)
            );
        }
        Err(err) if remote_trace_dir_missing(&err) => {
            println!("  {name}: no structured traces found");
        }
        Err(err) if remote_trace_table_missing(&err) => {
            println!("  {name}: requested trace tables not found");
        }
        Err(err) => return Err(err),
    }

    Ok(())
}

fn unpack_trace_archive(archive_path: &Path, traces_dir: &Path) -> Result<()> {
    if traces_dir.exists() {
        std::fs::remove_dir_all(traces_dir)?;
    }
    std::fs::create_dir_all(traces_dir)?;

    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(traces_dir)
        .status()?;

    if !status.success() {
        anyhow::bail!(
            "failed to unpack trace archive into {}",
            traces_dir.display()
        );
    }

    Ok(())
}

fn remote_trace_dir_missing(err: &anyhow::Error) -> bool {
    err.to_string().contains(TRACE_MISSING_MARKER)
}

fn remote_trace_table_missing(err: &anyhow::Error) -> bool {
    err.to_string().contains(TRACE_TABLE_MISSING_MARKER)
}

fn download_context(
    nodes: &str,
    directory: &str,
    data_subdir: Option<&str>,
) -> Result<(Vec<crate::config::Instance>, String, std::path::PathBuf)> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.miners, nodes)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    let data_dir = match data_subdir {
        Some(sub) => dir.join("data").join(sub),
        None => dir.join("data"),
    };
    std::fs::create_dir_all(&data_dir)?;

    Ok((targets, key, data_dir))
}

fn build_trace_download_script(selection: &TraceSelection) -> String {
    let request_mode = match selection {
        TraceSelection::All => "all".to_owned(),
        TraceSelection::Tables(tables) => format!(
            "selected\nrequested_files=({})",
            tables
                .iter()
                .map(|table| format!("\"{}\"", table.file_name()))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };

    format!(
        r#"set -e
if [ -f /root/payload/vars.sh ]; then
    # shellcheck disable=SC1091
    . /root/payload/vars.sh
fi

set -o pipefail
request_mode="{request_mode}"
found_any=0

declare -a selected_paths=()
declare -A selected_names=()

add_selected_path() {{
    path="$1"
    file="$(basename "$path")"
    if [ -n "${{selected_names[$file]+x}}" ]; then
        return
    fi
    selected_paths+=("$path")
    selected_names["$file"]=1
    found_any=1
}}

candidate_dirs=()
for var_name in ${{!ZEBRA@}}; do
    case "$var_name" in
        ZEBRA_*TRACE*_DIR|ZEBRA_*TRACING*_DIR)
            dir="${{!var_name}}"
            if [ -n "$dir" ]; then
                candidate_dirs+=("$dir")
            fi
            ;;
        ZEBRA_*TRACE*_FILE|ZEBRA_*TRACING*_FILE)
            file="${{!var_name}}"
            if [ -n "$file" ]; then
                candidate_dirs+=("$(dirname "$file")/traces")
            fi
            ;;
    esac
done
if [ -n "${{KRESKO_TRACE_DIR:-}}" ]; then
    candidate_dirs+=("$KRESKO_TRACE_DIR")
fi
if [ -d /root/.cache/zebra/traces ]; then
    candidate_dirs+=("/root/.cache/zebra/traces")
fi
if [ -d /root/.cache/kresko/txblast-traces ]; then
    candidate_dirs+=("/root/.cache/kresko/txblast-traces")
fi

if [ "$request_mode" = "all" ]; then
    for dir in "${{candidate_dirs[@]}}"; do
        [ -d "$dir" ] || continue
        while IFS= read -r -d '' path; do
            add_selected_path "$path"
        done < <(find "$dir" -maxdepth 1 -type f -print0 | sort -z)
    done
else
    for file in "${{requested_files[@]}}"; do
        found_path=""
        for dir in "${{candidate_dirs[@]}}"; do
            if [ -f "$dir/$file" ]; then
                found_path="$dir/$file"
                break
            fi
        done

        if [ -n "$found_path" ]; then
            add_selected_path "$found_path"
            continue
        fi

        trace_file="$(find /root /tmp -maxdepth 6 -type f -name "$file" -print -quit 2>/dev/null || true)"
        if [ -n "$trace_file" ]; then
            add_selected_path "$trace_file"
        fi
    done
fi

if [ "$found_any" -eq 0 ]; then
    if [ "${{#candidate_dirs[@]}}" -eq 0 ]; then
        echo "{trace_missing}" >&2
        exit 3
    fi
    echo "{trace_table_missing}" >&2
    exit 4
fi

manifest="$(mktemp /tmp/kresko-trace-files.XXXXXX)"
tar_stderr="$(mktemp /tmp/kresko-trace-tar-stderr.XXXXXX)"
trap 'rm -f "$manifest" "$tar_stderr"' EXIT
printf '%s\0' "${{selected_paths[@]}}" > "$manifest"

set +e
tar --ignore-failed-read --null -T "$manifest" --transform='flags=r;s|.*/||' -cf - 2>"$tar_stderr" | gzip -1
pipeline_status=("${{PIPESTATUS[@]}}")
set -e

tar_status="${{pipeline_status[0]}}"
gzip_status="${{pipeline_status[1]}}"
tar_ok=0
if [ "$tar_status" -eq 0 ]; then
    tar_ok=1
elif [ "$tar_status" -eq 1 ] && [ -s "$tar_stderr" ] && ! grep -Eqv '^tar: (.*: file changed as we read it|Removing leading .* from (member names|hard link targets))$' "$tar_stderr"; then
    tar_ok=1
fi

if [ "$tar_ok" -eq 1 ] && [ "$gzip_status" -eq 0 ]; then
    exit 0
fi

# Active JSONL trace files can grow while tar is streaming them. The archive is
# still usable, so only downgrade tar's warning status for that exact warning.
if [ "$tar_ok" -ne 1 ]; then
    cat "$tar_stderr" >&2
fi
if [ "$gzip_status" -ne 0 ]; then
    echo "gzip failed with status $gzip_status" >&2
fi

if [ "$tar_ok" -ne 1 ]; then
    exit "$tar_status"
fi
exit "$gzip_status"
"#,
        trace_missing = TRACE_MISSING_MARKER,
        trace_table_missing = TRACE_TABLE_MISSING_MARKER,
        request_mode = request_mode,
    )
}

#[cfg(test)]
mod tests {
    use super::{TraceSelection, TraceTable, build_trace_download_script};

    #[test]
    fn parse_all_selects_directory_mode() {
        assert_eq!(TraceTable::parse_list("all").unwrap(), TraceSelection::All);
    }

    #[test]
    fn all_trace_script_walks_candidate_directories() {
        let script = build_trace_download_script(&TraceSelection::All);
        assert!(script.contains("request_mode=\"all\""));
        assert!(script.contains("for var_name in ${!ZEBRA@}; do"));
        assert!(script.contains("ZEBRA_*TRACE*_DIR|ZEBRA_*TRACING*_DIR"));
        assert!(script.contains("find \"$dir\" -maxdepth 1 -type f -print0 | sort -z"));
        assert!(script.contains("tar --ignore-failed-read --null -T \"$manifest\""));
        assert!(script.contains("| gzip -1"));
        assert!(script.contains("file changed as we read it"));
    }
}
