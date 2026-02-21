//! Diagnostic tool to debug `get_package_diff` loop behavior.
//!
//! For each package in the workspace, this walks git history backwards
//! (like release-plz does) but WITHOUT using tags as a stopping mechanism.
//! It reports at each commit whether `are_packages_equal` / `are_cargo_toml_equal`
//! / `is_readme_updated` would match, and whether `published_at_sha1` would stop.
//!
//! Usage:
//!   cargo run --bin diff_debug -- /path/to/workspace [--package <name>]

use std::{collections::HashSet, path::Path};

use anyhow::Context;
use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::Package;
use cargo_utils::get_manifest_metadata;
use git_cmd::Repo;
use tracing_subscriber::EnvFilter;

use release_plz_core::{
    PackagePath as _, Publishable as _, ReleaseMetadata, ReleaseMetadataBuilder,
    are_packages_equal, is_readme_updated,
    registry_packages::get_registry_packages,
    registry_packages::RegistryPackage,
};

struct AlwaysRelease;
impl ReleaseMetadataBuilder for AlwaysRelease {
    fn get_release_metadata(&self, _package_name: &str) -> Option<ReleaseMetadata> {
        Some(ReleaseMetadata {
            tag_name_template: None,
            release_name_template: None,
        })
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: diff_debug <workspace-path> [--package <name>] [--simulate-unpublished]");
        std::process::exit(1);
    }

    let workspace_path = Utf8PathBuf::from(&args[1]);
    let manifest_path = workspace_path.join("Cargo.toml");

    // Parse flags from remaining args
    let mut single_package: Option<String> = None;
    let mut simulate_unpublished = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--package" => {
                i += 1;
                single_package = Some(args[i].clone());
            }
            "--simulate-unpublished" => {
                simulate_unpublished = true;
            }
            "--no-pub-sha1" => {
                // Will be handled below
            }
            _ => {}
        }
        i += 1;
    }
    let single_package_ref = single_package.as_deref();

    println!("=== diff_debug: analyzing workspace at {workspace_path} ===");
    if simulate_unpublished {
        println!("  MODE: --simulate-unpublished (pretending local_tag does not exist)");
    }
    println!();

    let metadata = get_manifest_metadata(&manifest_path)?;
    let project = release_plz_core::Project::new(
        &manifest_path,
        single_package_ref,
        &HashSet::new(),
        &metadata,
        &AlwaysRelease,
    )?;

    let publishable: Vec<&Package> = project
        .publishable_packages()
        .into_iter()
        .filter(|p| p.is_publishable())
        .collect();

    println!("Found {} publishable packages\n", publishable.len());

    // Download registry packages
    let pkg_refs: Vec<&Package> = publishable.iter().copied().collect();
    println!("Downloading registry packages...");
    let registry_packages = get_registry_packages(None, &pkg_refs, None)?;
    println!("Done downloading.\n");

    // Copy the project to a temp dir to avoid altering it
    let tmp_project_root_parent = release_plz_core::copy_to_temp_dir(project.root())?;
    let tmp_project_root =
        release_plz_core::new_project_root(project.root(), tmp_project_root_parent.path())?;
    let repository = Repo::new(&tmp_project_root)?;

    // Revert any dirty Cargo.lock from the original repo copy
    drop(repository.checkout("Cargo.lock"));

    for package in &publishable {
        println!("============================================================");
        println!("PACKAGE: {} v{}", package.name, package.version);
        println!("============================================================");

        let reg_pkg = registry_packages.get_registry_package(&package.name);

        match reg_pkg {
            None => {
                println!("  NOT in registry (new package). Skipping.\n");
                continue;
            }
            Some(rp) => {
                println!("  Registry version: {}", rp.package.version);
                println!("  published_at_sha1: {:?}", rp.published_at_sha1());
                let version_cmp = if package.version > rp.package.version {
                    "LOCAL > REGISTRY (version already bumped / unpublished)"
                } else if package.version == rp.package.version {
                    "LOCAL == REGISTRY (published)"
                } else {
                    "LOCAL < REGISTRY (unexpected!)"
                };
                println!("  Version status: {version_cmp}");

                // Compute tags
                let local_tag =
                    project.git_tag(&package.name, &package.version.to_string())?;
                let local_tag_commit = repository.get_tag_commit(&local_tag);
                println!(
                    "  Tag for local version ({local_tag}): {}",
                    local_tag_commit.as_deref().unwrap_or("NOT FOUND")
                );

                let registry_tag =
                    project.git_tag(&package.name, &rp.package.version.to_string())?;
                let registry_tag_commit = repository.get_tag_commit(&registry_tag);
                println!(
                    "  Tag for registry version ({registry_tag}): {}",
                    registry_tag_commit.as_deref().unwrap_or("NOT FOUND")
                );

                let registry_package_path = rp.package.package_path()?;

                // Now walk git history like get_package_diff does
                // When simulating unpublished, pretend local_tag doesn't exist
                // (this is what happens when local version > registry version)
                let effective_local_tag = if simulate_unpublished {
                    println!("  [simulate-unpublished] Forcing local_tag_commit to None");
                    None
                } else {
                    local_tag_commit.as_deref().map(|s| s.to_string())
                };
                analyze_package_diff(
                    &project,
                    package,
                    rp,
                    &registry_package_path,
                    &repository,
                    effective_local_tag.as_deref(),
                    registry_tag_commit.as_deref(),
                )?;
            }
        }
        println!();
    }

    // checkout back to HEAD
    repository
        .checkout_head()
        .context("can't checkout head after analysis")?;

    println!("=== Done ===");
    Ok(())
}

fn analyze_package_diff(
    project: &release_plz_core::Project,
    package: &Package,
    reg_pkg: &RegistryPackage,
    registry_package_path: &Utf8Path,
    repository: &Repo,
    local_tag_commit: Option<&str>,
    registry_tag_commit: Option<&str>,
) -> anyhow::Result<()> {
    let package_path = get_package_path_in_repo(package, repository, project.root())?;
    let pathbufs = vec![package_path.clone()];
    let paths_to_check: Vec<&Path> = pathbufs.iter().map(|p| p.as_std_path()).collect();

    let is_unpublished = package.version > reg_pkg.package.version;

    // Checkout HEAD first
    repository.checkout_head()?;

    // Debug: print the paths we're checking
    println!("  package_path: {package_path}");
    println!("  repo dir: {}", repository.directory());
    println!("  original_branch: {}", repository.original_branch());
    for p in &paths_to_check {
        println!("  path_to_check: {}", p.display());
    }

    // Revert Cargo.lock before checkout to avoid dirty-state errors
    drop(repository.checkout("Cargo.lock"));

    // Go to last commit touching these paths
    if let Err(e) = repository.checkout_last_commit_at_paths(&paths_to_check) {
        println!("  ERROR: can't checkout last commit for paths: {e}");
        return Ok(());
    }
    let head_hash = repository.current_commit_hash().unwrap_or_default();
    println!("  checked out to: {head_hash}");

    // Debug: check what git log sees from this detached HEAD
    let path_str = package_path.as_str();
    let log_output = std::process::Command::new("git")
        .args(["log", "--format=%H", "-n", "5", "--", path_str])
        .current_dir(repository.directory().as_std_path())
        .output();
    match log_output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let count = stdout.lines().count();
            println!("  git log -n5 for package path: {count} commits found");
            for (j, line) in stdout.lines().enumerate() {
                println!("    [{j}] {}", &line[..8.min(line.len())]);
            }
        }
        Err(e) => println!("  git log debug failed: {e}"),
    }

    let max_commits = 200_u32;
    // current = what release-plz does now (local_tag + pub_sha1)
    // worst   = no local_tag AND no pub_sha1 → only content equality
    // proposed = with registry_tag fallback
    println!("\n  Walking git history (max {max_commits} commits):");
    println!(
        "  {:>4} | {:8} | {:12} | {:12} | {:>12} | {:>8} | {:>10} | {}",
        "#", "hash", "cargo_toml", "pkg_equal", "current_stop", "worst", "proposed", "commit_msg"
    );
    println!(
        "  {:-<4}-+-{:-<8}-+-{:-<12}-+-{:-<12}-+-{:-<12}-+-{:-<8}-+-{:-<10}-+-{:-<40}",
        "", "", "", "", "", "", "", ""
    );

    let mut current_first_stop: Option<u32> = None;
    let mut proposed_first_stop: Option<u32> = None;
    let mut equality_first_stop: Option<u32> = None;
    let mut worst_first_stop: Option<u32> = None;

    for i in 0..max_commits {
        let commit_hash = match repository.current_commit_hash() {
            Ok(h) => h,
            Err(e) => {
                println!("  [commit {i}] ERROR getting hash: {e}");
                break;
            }
        };
        let short_hash = &commit_hash[..8.min(commit_hash.len())];
        let commit_msg = repository.current_commit_message().unwrap_or_default();
        let first_line = commit_msg.lines().next().unwrap_or("").to_string();
        let short_msg: String = first_line.chars().take(50).collect();

        // 1. Check are_cargo_toml_equal
        let cargo_toml_eq = are_cargo_toml_equal_check(&package_path, registry_package_path);

        // 2. Check full are_packages_equal (only if cargo toml matched)
        let pkg_equal_str = if cargo_toml_eq {
            let result = are_packages_equal(&package_path, registry_package_path);
            // Revert Cargo.lock changes caused by `cargo package --list`
            drop(repository.checkout("Cargo.lock"));
            match result {
                Ok(true) => "EQUAL",
                Ok(false) => "diff(files)",
                Err(_e) => "ERR",
            }
        } else {
            // Even if we skip, cargo_toml_equal_check doesn't modify files,
            // but let's be safe
            "skip"
        };

        // 3. Check is_readme_updated
        let _readme_str = match is_readme_updated(
            &package.name,
            &package_path,
            registry_package_path,
        ) {
            Ok(true) => "YES",
            Ok(false) => "no",
            Err(_) => "err",
        };

        // 4. Check tag-based stopping
        let (current_stop, worst_stop, proposed_stop) = check_tag_stop(
            repository,
            local_tag_commit,
            registry_tag_commit,
            reg_pkg.published_at_sha1(),
            &commit_hash,
        );

        let cargo_toml_str = if cargo_toml_eq { "EQUAL" } else { "different" };
        let is_equal = cargo_toml_eq && pkg_equal_str == "EQUAL";

        println!(
            "  {:>4} | {short_hash} | {cargo_toml_str:12} | {pkg_equal_str:12} | {current_stop:>12} | {worst_stop:>8} | {proposed_stop:>10} | {short_msg}",
            i
        );

        // Track first stops
        if is_equal && equality_first_stop.is_none() {
            equality_first_stop = Some(i);
        }
        if (is_equal || current_stop != "no") && current_first_stop.is_none() {
            current_first_stop = Some(i);
        }
        if (is_equal || worst_stop != "no") && worst_first_stop.is_none() {
            worst_first_stop = Some(i);
        }
        if (is_equal || proposed_stop != "no") && proposed_first_stop.is_none() {
            proposed_first_stop = Some(i);
        }

        // Revert Cargo.lock before checkout to avoid dirty-state errors
        drop(repository.checkout("Cargo.lock"));

        // Go to previous commit
        match repository.checkout_previous_commit_at_paths(&paths_to_check) {
            Ok(()) => {}
            Err(e) => {
                // Debug: run git log manually to see what's happening
                let path_strs: Vec<&str> = paths_to_check.iter().map(|p| p.to_str().unwrap_or("")).collect();
                let mut cmd = std::process::Command::new("git");
                cmd.args(["log", "--format=%H", "-n", "2", "--"]);
                for p in &path_strs {
                    cmd.arg(p);
                }
                cmd.current_dir(repository.directory().as_std_path());
                let debug_out = cmd.output();
                let debug_info = match &debug_out {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        format!("stdout={stdout:?} stderr={stderr:?}")
                    }
                    Err(e2) => format!("cmd failed: {e2}"),
                };
                println!("  [no more commits after {i}] err={e:#}");
                println!("    debug git log: {debug_info}");
                // Check for dirty state that might block checkout
                let status_out = std::process::Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(repository.directory().as_std_path())
                    .output();
                if let Ok(out) = status_out {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    if !stdout.is_empty() {
                        println!("    dirty files: {}", stdout.trim());
                    }
                }
                break;
            }
        }
    }

    // Summary
    let fmt_stop = |v: Option<u32>| -> String {
        v.map(|n| format!("commit #{n}"))
            .unwrap_or_else(|| "NEVER".to_string())
    };
    println!();
    if is_unpublished {
        println!(
            "  >>> UNPUBLISHED VERSION (local {} > registry {})",
            package.version, reg_pkg.package.version
        );
    }
    println!(
        "  SUMMARY:\n    content_equality_stop = {}\n    current_behavior_stop = {}\n    worst_case_stop      = {} (no local_tag, no pub_sha1)\n    proposed_stop         = {} (with registry_tag fallback)",
        fmt_stop(equality_first_stop),
        fmt_stop(current_first_stop),
        fmt_stop(worst_first_stop),
        fmt_stop(proposed_first_stop),
    );
    if worst_first_stop.is_none() {
        println!("  !!! BUG: if pub_sha1 is missing, content-only checks NEVER stop the loop — traverses entire history !!!");
    }
    if worst_first_stop != equality_first_stop {
        println!(
            "  NOTE: worst_case differs from content_equality — there's a gap where tag stops but content doesn't"
        );
    }
    if current_first_stop.is_none() && is_unpublished {
        println!("  !!! BUG: current behavior would traverse entire history for this unpublished package !!!");
    }
    if proposed_first_stop.is_some() && current_first_stop.is_none() {
        println!(
            "  FIX: proposed behavior (using registry tag) would stop at commit #{}",
            proposed_first_stop.unwrap()
        );
    }

    // Restore HEAD
    repository.checkout_head()?;
    Ok(())
}

/// Simplified version of are_cargo_toml_equal from package_compare
fn are_cargo_toml_equal_check(local_package: &Utf8Path, registry_package: &Utf8Path) -> bool {
    let local_toml = local_package.join("Cargo.toml");
    let registry_toml_orig = registry_package.join("Cargo.toml.orig");

    let Ok(local_bytes) = std::fs::read(&local_toml) else {
        return false;
    };
    let Ok(registry_bytes) = std::fs::read(&registry_toml_orig) else {
        return false;
    };
    local_bytes == registry_bytes
}

/// Returns (current_behavior, worst_case, proposed_behavior)
/// current = what release-plz does now (only local_tag + pub_sha1)
/// worst   = no local_tag AND no pub_sha1 (content-only)
/// proposed = with registry_tag added as fallback
fn check_tag_stop(
    repository: &Repo,
    local_tag_commit: Option<&str>,
    registry_tag_commit: Option<&str>,
    published_at_sha1: Option<&str>,
    current_commit_hash: &str,
) -> (&'static str, &'static str, &'static str) {
    // === Current behavior (only local_tag + pub_sha1) ===
    let current = {
        let mut result = "no";
        if let Some(tc) = local_tag_commit {
            if repository.is_ancestor(current_commit_hash, tc) {
                result = "local_tag";
            }
        }
        if result == "no" {
            if let Some(sha) = published_at_sha1 {
                if repository.is_ancestor(current_commit_hash, sha) {
                    result = "pub_sha1";
                }
            }
        }
        result
    };

    // === Worst case: no local_tag AND no pub_sha1 (only content checks) ===
    let worst = "no"; // tag-based stop never fires; only content equality can save us

    // === Proposed behavior (registry_tag as fallback) ===
    let proposed = {
        let mut result = "no";
        if let Some(tc) = local_tag_commit {
            if repository.is_ancestor(current_commit_hash, tc) {
                result = "local_tag";
            }
        }
        if result == "no" {
            if let Some(tc) = registry_tag_commit {
                if repository.is_ancestor(current_commit_hash, tc) {
                    result = "reg_tag";
                }
            }
        }
        if result == "no" {
            if let Some(sha) = published_at_sha1 {
                if repository.is_ancestor(current_commit_hash, sha) {
                    result = "pub_sha1";
                }
            }
        }
        result
    };

    (current, worst, proposed)
}

fn get_package_path_in_repo(
    package: &Package,
    repository: &Repo,
    project_root: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    let package_path = package.package_path()?;
    let relative_path = release_plz_core::fs_utils::strip_prefix(&package_path, project_root)?;
    Ok(repository.directory().join(relative_path))
}
