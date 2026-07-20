//! Deterministic file and device tooling for the GoXLR Home Manager module.
//!
//! The native GoXLR profile formats are intentionally treated as opaque,
//! validated artifacts here.  This keeps the first declarative surface
//! lossless while the upstream profile model evolves.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST_VERSION: u64 = 1;
const PLAN_SCHEMA: &str = "goxlr-config.plan/v1";
const OBSERVATION_SCHEMA: &str = "goxlr-config.observation/v1";

#[derive(Debug, Parser)]
#[command(about = "Deterministic GoXLR configuration and reconciliation tooling")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Capture the live GoXLR daemon status through its supported CLI/API.
    Discover {
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Compare declared source artifacts with their mutable runtime targets.
    Plan {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Copy declared artifacts atomically and verify every resulting hash.
    Apply {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Plan by default; apply only when the explicit flag is present.
    Reconcile {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
    /// Verify that all declared runtime targets match their sources.
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: u64,
    module: String,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: String,
    source: PathBuf,
    target: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum FileStatus {
    Same,
    Missing,
    Changed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    schema: &'static str,
    module: String,
    manifest_version: u64,
    requires_apply: bool,
    files: Vec<PlanFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanFile {
    path: String,
    source: String,
    target: String,
    status: FileStatus,
    source_sha256: Option<String>,
    target_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    schema: &'static str,
    device: Option<String>,
    status: Value,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        CommandKind::Discover { device, json } => discover(device.as_deref(), json),
        CommandKind::Plan { manifest, json } => print_plan(&read_plan(&manifest)?, json),
        CommandKind::Apply { manifest, json } => {
            let plan = read_plan(&manifest)?;
            apply(&plan)?;
            let result = read_plan(&manifest)?;
            if json {
                print_plan(&result, true)
            } else {
                println!("applied {} file(s)", changed_files(&plan));
                Ok(())
            }
        }
        CommandKind::Reconcile {
            manifest,
            apply: should_apply,
            json,
        } => {
            let plan = read_plan(&manifest)?;
            if should_apply {
                apply(&plan)?;
            }
            let result = if should_apply {
                read_plan(&manifest)?
            } else {
                plan
            };
            print_plan(&result, json)
        }
        CommandKind::Verify { manifest, json } => {
            let plan = read_plan(&manifest)?;
            let changed = changed_files(&plan);
            if json {
                print_plan(&plan, true)?;
            } else if changed == 0 {
                println!("verified: all declared GoXLR files match");
            } else {
                println!("verification failed: {changed} file(s) differ");
            }
            if changed == 0 {
                Ok(())
            } else {
                bail!("declared GoXLR files differ from runtime targets")
            }
        }
    }
}

fn discover(device: Option<&str>, json_output: bool) -> Result<()> {
    let mut command = Command::new("goxlr-client");
    command.arg("--status-json");
    if let Some(device) = device {
        command.args(["--device", device]);
    }
    let output = command
        .output()
        .context("failed to execute goxlr-client --status-json")?;
    if !output.status.success() {
        bail!(
            "goxlr-client exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let status: Value =
        serde_json::from_slice(&output.stdout).context("goxlr-client returned invalid JSON")?;
    let observation = Observation {
        schema: OBSERVATION_SCHEMA,
        device: device.map(str::to_owned),
        status,
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&observation)?);
    } else {
        let mixers = observation
            .status
            .get("mixers")
            .and_then(Value::as_object)
            .map_or(0, |mixers| mixers.len());
        println!("{}: {mixers} mixer(s) reported", observation.schema);
    }
    Ok(())
}

fn read_plan(path: &Path) -> Result<Plan> {
    let manifest: Manifest = serde_json::from_reader(
        File::open(path).with_context(|| format!("cannot open manifest {}", path.display()))?,
    )
    .with_context(|| format!("cannot parse manifest {}", path.display()))?;
    validate_manifest(&manifest)?;

    let files = manifest
        .files
        .iter()
        .map(inspect_manifest_file)
        .collect::<Result<Vec<_>>>()?;
    let requires_apply = files.iter().any(|file| file.status != FileStatus::Same);
    Ok(Plan {
        schema: PLAN_SCHEMA,
        module: manifest.module,
        manifest_version: manifest.version,
        requires_apply,
        files,
    })
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.version != MANIFEST_VERSION {
        bail!("unsupported manifest version {}", manifest.version);
    }
    if manifest.module.trim().is_empty() {
        bail!("manifest module must not be empty");
    }
    let mut targets = BTreeSet::new();
    for file in &manifest.files {
        if file.path.is_empty() || Path::new(&file.path).is_absolute() {
            bail!(
                "manifest path must be non-empty and relative: {}",
                file.path
            );
        }
        if !file.source.is_file() {
            bail!("manifest source does not exist: {}", file.source.display());
        }
        if !file.target.is_absolute() {
            bail!(
                "manifest target must be absolute: {}",
                file.target.display()
            );
        }
        if !targets.insert(file.target.clone()) {
            bail!(
                "manifest contains duplicate target: {}",
                file.target.display()
            );
        }
    }
    Ok(())
}

fn apply(plan: &Plan) -> Result<()> {
    if !plan.requires_apply {
        return Ok(());
    }
    enum JournalEntry {
        Created { target: PathBuf },
        Replaced { backup: PathBuf, target: PathBuf },
    }
    let mut journal = Vec::new();
    let result = (|| {
        for (index, file) in plan.files.iter().enumerate() {
            if file.status == FileStatus::Same {
                continue;
            }
            let source = Path::new(&file.source);
            let target = Path::new(&file.target);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            if target.exists() {
                let backup = backup_path(target, index)?;
                fs::rename(target, &backup).with_context(|| {
                    format!("cannot stage previous target {}", target.display())
                })?;
                journal.push(JournalEntry::Replaced {
                    backup,
                    target: target.to_path_buf(),
                });
            } else {
                journal.push(JournalEntry::Created {
                    target: target.to_path_buf(),
                });
            }
            atomic_copy(source, target)?;
        }
        let verified = read_plan_from_plan(plan)?;
        if verified.requires_apply {
            bail!("post-apply verification found changed files");
        }
        Ok::<(), anyhow::Error>(())
    })();
    match result {
        Ok(()) => {
            for entry in journal {
                if let JournalEntry::Replaced { backup, .. } = entry {
                    let _ = fs::remove_file(backup);
                }
            }
            Ok(())
        }
        Err(error) => {
            for entry in journal.into_iter().rev() {
                match entry {
                    JournalEntry::Created { target } => {
                        let _ = fs::remove_file(target);
                    }
                    JournalEntry::Replaced { backup, target } => {
                        let _ = fs::remove_file(&target);
                        let _ = fs::rename(backup, target);
                    }
                }
            }
            Err(error)
        }
    }
}

fn read_plan_from_plan(plan: &Plan) -> Result<Plan> {
    let files = plan
        .files
        .iter()
        .map(inspect_plan_file)
        .collect::<Result<Vec<_>>>()?;
    Ok(Plan {
        schema: PLAN_SCHEMA,
        module: plan.module.clone(),
        manifest_version: plan.manifest_version,
        requires_apply: files.iter().any(|file| file.status != FileStatus::Same),
        files,
    })
}

fn inspect_manifest_file(file: &ManifestFile) -> Result<PlanFile> {
    let source_sha256 = Some(sha256(&file.source)?);
    let target_sha256 = sha256_optional(&file.target)?;
    Ok(plan_file(
        file.path.clone(),
        file.source.display().to_string(),
        file.target.display().to_string(),
        source_sha256,
        target_sha256,
    ))
}

fn inspect_plan_file(file: &PlanFile) -> Result<PlanFile> {
    let source_sha256 = Some(sha256(Path::new(&file.source))?);
    let target_sha256 = sha256_optional(Path::new(&file.target))?;
    Ok(plan_file(
        file.path.clone(),
        file.source.clone(),
        file.target.clone(),
        source_sha256,
        target_sha256,
    ))
}

fn plan_file(
    path: String,
    source: String,
    target: String,
    source_sha256: Option<String>,
    target_sha256: Option<String>,
) -> PlanFile {
    let status = match (&source_sha256, &target_sha256) {
        (Some(source), Some(target)) if source == target => FileStatus::Same,
        (Some(_), Some(_)) => FileStatus::Changed,
        _ => FileStatus::Missing,
    };
    PlanFile {
        path,
        source,
        target,
        status,
        source_sha256,
        target_sha256,
    }
}

fn sha256_optional(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(bytes))
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn backup_path(target: &Path, index: usize) -> Result<PathBuf> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("target has no valid file name: {}", target.display()))?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = parent.join(format!(".{name}.canix-previous-{}-{index}", suffix));
    if backup.exists() {
        bail!("refusing to overwrite staged backup {}", backup.display());
    }
    Ok(backup)
}

fn atomic_copy(source: &Path, target: &Path) -> Result<()> {
    let bytes = fs::read(source).with_context(|| format!("cannot read {}", source.display()))?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = PathBuf::from(format!("{}.canix-tmp-{suffix}", target.display()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("cannot create {}", temp.display()))?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        if let Ok(metadata) = fs::metadata(source) {
            fs::set_permissions(&temp, metadata.permissions())?;
        }
        fs::rename(&temp, target)
            .with_context(|| format!("cannot install {}", target.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn sha256(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn changed_files(plan: &Plan) -> usize {
    plan.files
        .iter()
        .filter(|file| file.status != FileStatus::Same)
        .count()
}

fn print_plan(plan: &Plan, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(plan)?);
        return Ok(());
    }
    if plan.requires_apply {
        println!(
            "{}: {} file(s) require reconciliation",
            plan.schema,
            changed_files(plan)
        );
        for file in &plan.files {
            if file.status != FileStatus::Same {
                println!("{:?} {}", file.status, file.path);
            }
        }
    } else {
        println!("{}: no changes", plan.schema);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_deterministic_and_distinguishes_missing_targets() {
        let root = std::env::temp_dir().join(format!(
            "goxlr-config-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("test root should be creatable");
        let source = root.join("profile.goxlr");
        let target = root.join("runtime/profile.goxlr");
        let manifest = root.join("manifest.json");
        fs::write(&source, b"opaque profile bytes\n").expect("source should be writable");
        fs::write(
            &manifest,
            serde_json::json!({
                "version": MANIFEST_VERSION,
                "module": "programs.goxlr-utility",
                "files": [{
                    "path": "profiles/profile.goxlr",
                    "source": source,
                    "target": target,
                }],
            })
            .to_string(),
        )
        .expect("manifest should be writable");

        let plan = read_plan(&manifest).expect("manifest should parse");
        assert!(plan.requires_apply);
        assert_eq!(plan.files[0].status, FileStatus::Missing);

        fs::remove_dir_all(root).expect("test root should be removable");
    }

    #[test]
    fn apply_restores_replaced_targets_when_a_later_copy_fails() {
        let root = std::env::temp_dir().join(format!(
            "goxlr-config-rollback-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("test root should be creatable");
        let source = root.join("source");
        let target = root.join("target");
        fs::write(&source, b"new").expect("source should be writable");
        fs::write(&target, b"old").expect("target should be writable");
        let missing = root.join("missing");
        let plan = Plan {
            schema: PLAN_SCHEMA,
            module: "test".into(),
            manifest_version: MANIFEST_VERSION,
            requires_apply: true,
            files: vec![
                plan_file(
                    "first".into(),
                    source.display().to_string(),
                    target.display().to_string(),
                    Some(sha256(&source).expect("source hash")),
                    Some(sha256(&target).expect("target hash")),
                ),
                plan_file(
                    "second".into(),
                    missing.display().to_string(),
                    root.join("second-target").display().to_string(),
                    Some("sha256:deadbeef".into()),
                    None,
                ),
            ],
        };

        assert!(apply(&plan).is_err());
        assert_eq!(fs::read(&target).expect("target should remain"), b"old");
        assert!(!root.join("second-target").exists());
        fs::remove_dir_all(root).expect("test root should be removable");
    }
}
