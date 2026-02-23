use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::Context as _;
use cargo::util::VersionExt as _;
use cargo_metadata::{
    Package, TargetKind,
    camino::{Utf8Path, Utf8PathBuf},
    semver::{self, Version},
};
use cargo_utils::{CARGO_TOML, LocalManifest};
use git_cliff_core::{
    config::{ChangelogConfig, Config},
    contributor::RemoteContributor,
};
use git_cmd::Repo;
use next_version::NextVersion as _;
use rayon::iter::{IntoParallelRefMutIterator as _, ParallelIterator as _};
use std::sync::Once;
use tracing::{debug, info, instrument, warn};

use crate::{
    ChangelogBuilder, ChangelogRequest, NO_COMMIT_ID, PackagePath as _, Project, Remote, RepoUrl,
    UpdateResult,
    changelog_filler::{fill_commit, get_required_info},
    changelog_parser,
    command::update::changelog_update::OldChangelogs,
    diff::{Commit, Diff},
    fs_utils, lock_compare,
    registry_packages::{PackagesCollection, RegistryPackage},
    semver_check::{self, SemverCheck},
    toml_compare,
    version::NextVersionFromDiff as _,
};

use crate::version::BumpLevel;

use super::{
    PackagesToUpdate, PackagesUpdate,
    package_dependencies::PackageDependencies as _,
    update_request::{ReleaseMode, UpdateRequest},
};

static SEMVER_CHECK_LOG_ONCE: Once = Once::new();

#[derive(Debug)]
pub struct Updater<'a> {
    pub project: &'a Project,
    pub req: &'a UpdateRequest,
}

impl Updater<'_> {
    #[instrument(skip_all)]
    pub async fn packages_to_update(
        &self,
        registry_packages: &PackagesCollection,
        repository: &Repo,
        local_manifest_path: &Utf8Path,
    ) -> anyhow::Result<PackagesUpdate> {
        debug!("calculating local packages");

        let packages_diffs = self
            .get_packages_diffs(registry_packages, repository)
            .await?;

        match self.req.release_mode() {
            ReleaseMode::Rc | ReleaseMode::Stable => {
                self.packages_to_update_rc_stable(&packages_diffs, repository)
            }
            ReleaseMode::Default => {
                self.packages_to_update_default(packages_diffs, local_manifest_path)
            }
        }
    }

    /// Default mode: existing single-pass behavior, unchanged.
    fn packages_to_update_default(
        &self,
        packages_diffs: Vec<(&Package, Diff)>,
        local_manifest_path: &Utf8Path,
    ) -> anyhow::Result<PackagesUpdate> {
        let version_groups = self.get_version_groups(&packages_diffs)?;
        debug!("version groups: {:?}", version_groups);

        let mut packages_to_check_for_deps: Vec<&Package> = vec![];
        let mut packages_to_update = PackagesUpdate::default();

        let workspace_version_pkgs: HashSet<String> = packages_diffs
            .iter()
            .filter(|(p, _)| {
                let local_manifest_path = p.package_path().unwrap().join(CARGO_TOML);
                let local_manifest = LocalManifest::try_new(&local_manifest_path).unwrap();
                local_manifest.version_is_inherited()
            })
            .map(|(p, _)| p.name.to_string())
            .collect();

        let new_workspace_version = self.new_workspace_version(
            local_manifest_path,
            &packages_diffs,
            &workspace_version_pkgs,
        )?;
        if let Some(new_workspace_version) = &new_workspace_version {
            packages_to_update.with_workspace_version(new_workspace_version.clone());
        }

        let mut old_changelogs = OldChangelogs::new();
        for (p, diff) in packages_diffs {
            if let Some(release_commits_regex) = self.req.release_commits()
                && !diff.any_commit_matches(release_commits_regex)
            {
                info!("{}: no commit matches the `release_commits` regex", p.name);
                // We need to update this package only if one of its dependencies has changed.
                packages_to_check_for_deps.push(p);
                continue;
            }
            let next_version = self.get_next_version(
                new_workspace_version.as_ref(),
                p,
                &workspace_version_pkgs,
                &version_groups,
                &diff,
            )?;
            debug!(
                "package: {}, diff: {diff:?}, next_version: {next_version}",
                p.name,
            );
            let current_version = p.version.clone();
            // Process package if:
            // - Version changes.
            // - Package is new.
            // - Version was already bumped with pending unreleased commits so that we update the changelog.
            let version_already_bumped = !diff.is_version_published && !diff.commits.is_empty();
            if next_version != current_version
                || !diff.registry_package_exists
                || version_already_bumped
            {
                if version_already_bumped {
                    info!(
                        "{}: updating changelog for version {current_version}{}",
                        p.name,
                        diff.semver_check.outcome_str()
                    );
                } else {
                    info!(
                        "{}: next version is {next_version}{}",
                        p.name,
                        diff.semver_check.outcome_str()
                    );
                }
                let update_result = self.calculate_update_result(
                    diff.commits,
                    next_version,
                    p,
                    diff.semver_check,
                    diff.registry_version,
                    &mut old_changelogs,
                )?;
                packages_to_update
                    .updates_mut()
                    .push((p.clone(), update_result));
            } else if diff.is_version_published {
                // We need to update this package only if one of its dependencies has changed.
                packages_to_check_for_deps.push(p);
            }
        }

        let changed_packages: Vec<(&Package, Version)> = packages_to_update
            .updates()
            .iter()
            .map(|(p, u)| (p, u.version.clone()))
            .collect();
        let dependent_packages =
            self.dependent_packages_update(&packages_to_check_for_deps, &changed_packages)?;
        packages_to_update.updates_mut().extend(dependent_packages);
        Ok(packages_to_update)
    }

    /// RC/Stable mode: two-pass version calculation with transitive bump propagation.
    fn packages_to_update_rc_stable(
        &self,
        packages_diffs: &[(&Package, Diff)],
        repository: &Repo,
    ) -> anyhow::Result<PackagesUpdate> {
        let release_mode = self.req.release_mode();
        let mut packages_to_update = PackagesUpdate::default();

        // ── Pass 1: per-package bump from own commits ──
        // Collect (package, diff, own_bump, base_version) for each package with changes.
        struct Pass1Entry<'a> {
            package: &'a Package,
            diff: Diff,
            own_bump: BumpLevel,
            base_version: Version,
        }

        let mut pass1: Vec<Pass1Entry<'_>> = Vec::new();

        for (p, diff) in packages_diffs {
            let base_version = match &diff.base_version {
                Some(v) => v.clone(),
                // No stable tag found — use current version as base.
                None => p.version.clone(),
            };

            if diff.commits.is_empty() && diff.registry_package_exists {
                // No own commits — own_bump is None, but we still track it
                // for potential dependency-triggered bumps.
                pass1.push(Pass1Entry {
                    package: p,
                    diff: diff.clone(),
                    own_bump: BumpLevel::None,
                    base_version,
                });
                continue;
            }

            if !diff.registry_package_exists {
                // New package — include as-is with no bump (it uses its Cargo.toml version).
                pass1.push(Pass1Entry {
                    package: p,
                    diff: diff.clone(),
                    own_bump: BumpLevel::None,
                    base_version,
                });
                continue;
            }

            // Calculate bump level from commits against base_version.
            let pkg_config = self.req.get_package_config(&p.name);
            let version_updater = pkg_config.generic.version_updater()?;
            let next_from_commits =
                version_updater.increment(&base_version, diff.commits.iter().map(|c| &c.message));
            let own_bump = BumpLevel::compute(&base_version, &next_from_commits);

            pass1.push(Pass1Entry {
                package: p,
                diff: diff.clone(),
                own_bump,
                base_version,
            });
        }

        // ── Transitive propagation (topological order, leaves-first) ──
        // Build entries for the propagation function.
        let prop_entries: Vec<(String, BumpLevel, Vec<String>)> = pass1
            .iter()
            .map(|e| {
                let dep_names = e
                    .package
                    .dependencies
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.kind,
                            cargo_metadata::DependencyKind::Normal
                                | cargo_metadata::DependencyKind::Build
                        )
                    })
                    .map(|d| String::from(d.name.as_str()))
                    .collect();
                (e.package.name.to_string(), e.own_bump, dep_names)
            })
            .collect();

        // Topological sort: the packages in `self.project.publishable_packages()` are already
        // in release order (dependencies before dependents) from `release_order()`.
        let ordered_names: Vec<String> = self
            .packages_to_process()
            .iter()
            .map(|p| p.name.to_string())
            .collect();

        let final_bumps = propagate_bumps(&prop_entries, &ordered_names);

        // ── Pass 2: compute final versions and generate changelogs ──
        let mut old_changelogs = OldChangelogs::new();

        for entry in pass1 {
            let final_bump = final_bumps
                .get(entry.package.name.as_str())
                .copied()
                .unwrap_or(BumpLevel::None);

            if final_bump == BumpLevel::None && entry.diff.registry_package_exists {
                // No bump needed — skip this package unless it's new.
                continue;
            }

            let target_stable = if entry.diff.registry_package_exists {
                final_bump.apply(&entry.base_version)
            } else {
                // New package — use its Cargo.toml version as-is.
                entry.package.version.clone()
            };

            let final_version = match release_mode {
                ReleaseMode::Rc => {
                    let rc_num = find_next_rc_number(
                        self.project,
                        &entry.package.name,
                        &target_stable,
                        repository,
                    );
                    let pre_str = format!("rc.{rc_num}");
                    let pre = semver::Prerelease::new(&pre_str)
                        .context("failed to create RC prerelease")?;
                    Version {
                        pre,
                        ..target_stable
                    }
                }
                ReleaseMode::Stable => target_stable,
                ReleaseMode::Default => unreachable!(),
            };

            info!(
                "{}: {} -> {}",
                entry.package.name, entry.base_version, final_version
            );

            let update_result = self.calculate_update_result(
                entry.diff.commits,
                final_version,
                entry.package,
                entry.diff.semver_check,
                entry.diff.registry_version,
                &mut old_changelogs,
            )?;
            packages_to_update
                .updates_mut()
                .push((entry.package.clone(), update_result));
        }

        Ok(packages_to_update)
    }

    /// Get the highest next version of all packages for each version group.
    fn get_version_groups(
        &self,
        packages_diffs: &[(&Package, Diff)],
    ) -> anyhow::Result<HashMap<String, Version>> {
        let mut version_groups: HashMap<String, Version> = HashMap::new();

        for (pkg, diff) in packages_diffs {
            let pkg_config = self.req.get_package_config(&pkg.name);
            let version_updater = pkg_config.generic.version_updater()?;
            if let Some(version_group) = pkg_config.version_group {
                let base = diff.base_version.as_ref().unwrap_or(&pkg.version);
                let next_pkg_ver = base.next_from_diff(diff, version_updater);
                match version_groups.entry(version_group.clone()) {
                    std::collections::hash_map::Entry::Occupied(v) => {
                        // maximum version of the group until now
                        let max = v.get();
                        if max < &next_pkg_ver {
                            version_groups.insert(version_group, next_pkg_ver);
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(_) => {
                        version_groups.insert(version_group, next_pkg_ver);
                    }
                }
            }
        }

        Ok(version_groups)
    }

    fn new_workspace_version(
        &self,
        local_manifest_path: &Utf8Path,
        packages_diffs: &[(&Package, Diff)],
        workspace_version_pkgs: &HashSet<String>,
    ) -> anyhow::Result<Option<Version>> {
        let workspace_version = {
            let local_manifest = LocalManifest::try_new(local_manifest_path)?;
            local_manifest.get_workspace_version()
        };
        let mut new_versions = Vec::new();
        for workspace_package in workspace_version_pkgs {
            for (p, diff) in packages_diffs {
                if *workspace_package == *p.name {
                    let pkg_config = self.req.get_package_config(&p.name);
                    let version_updater = pkg_config.generic.version_updater()?;
                    let base = diff.base_version.as_ref().unwrap_or(&p.version);
                    let next = base.next_from_diff(diff, version_updater);
                    if let Some(workspace_version) = &workspace_version
                        && &next >= workspace_version
                    {
                        new_versions.push(next);
                    }
                }
            }
        }
        Ok(new_versions.into_iter().max())
    }

    async fn get_packages_diffs(
        &self,
        registry_packages: &PackagesCollection,
        repository: &Repo,
    ) -> anyhow::Result<Vec<(&Package, Diff)>> {
        // Store diff for each package. This operation is not thread safe, so we do it in one
        // package at a time.

        let packages_diffs_res: anyhow::Result<Vec<(&Package, Diff)>> = self
            .packages_to_process()
            .iter()
            .map(|&p| {
                let diff = self
                    .get_diff(p, registry_packages, repository)
                    .with_context(|| {
                        format!("failed to retrieve difference of package {}", p.name)
                    })?;
                Ok((p, diff))
            })
            .collect();

        let mut packages_diffs = self.fill_commits(&packages_diffs_res?, repository).await?;
        let packages_commits: HashMap<String, Vec<Commit>> = packages_diffs
            .iter()
            .map(|(p, d)| (p.name.to_string(), d.commits.clone()))
            .collect();

        let semver_check_result: anyhow::Result<()> =
            packages_diffs.par_iter_mut().try_for_each(|(p, diff)| {
                let registry_package = registry_packages.get_package(&p.name);
                if let Some(registry_package) = registry_package {
                    let package_path = get_package_path(p, repository, self.project.root())
                        .context("can't retrieve package path")?;
                    let package_config = self.req.get_package_config(&p.name);
                    for pkg_to_include in &package_config.changelog_include {
                        if let Some(commits) = packages_commits.get(pkg_to_include) {
                            diff.add_commits(commits);
                        }
                    }
                    if should_check_semver(p, package_config.semver_check())
                        && diff.should_update_version()
                    {
                        let registry_package_path = registry_package
                            .package_path()
                            .context("can't retrieve registry package path")?;
                        // Log that we are checking semver only the first time.
                        SEMVER_CHECK_LOG_ONCE.call_once(|| {
                            tracing::info!(
                                "Checking API compatibility with cargo-semver-checks..."
                            );
                        });
                        let semver_check =
                            semver_check::run_semver_check(&package_path, registry_package_path)
                                .context("error while running cargo-semver-checks")?;
                        diff.set_semver_check(semver_check);
                    }
                }
                Ok(())
            });
        semver_check_result?;

        Ok(packages_diffs)
    }

    fn packages_to_process(&self) -> Vec<&Package> {
        // Collect packages that are either publishable or git-only, with de-duplication, order is important.
        let mut packages_to_process: Vec<&Package> = Vec::new();
        let mut package_names: HashSet<String> = HashSet::new();

        // Add publishable packages
        for p in self.project.publishable_packages() {
            if package_names.insert(p.name.to_string()) {
                packages_to_process.push(p);
            }
        }

        // Add git-only packages, not already added
        for p in self.project.workspace_packages() {
            if self.req.should_use_git_only(&p.name) && package_names.insert(p.name.to_string()) {
                packages_to_process.push(p);
            }
        }
        packages_to_process
    }

    async fn fill_commits<'a>(
        &self,
        packages_diffs: &[(&'a Package, Diff)],
        repository: &Repo,
    ) -> anyhow::Result<Vec<(&'a Package, Diff)>> {
        let git_client = self.req.git_client()?;
        let changelog_request: &ChangelogRequest = self.req.changelog_req();
        let mut all_commits: HashMap<String, &Commit> = HashMap::new();
        let mut packages_diffs = packages_diffs.to_owned();
        if let Some(changelog_config) = changelog_request.changelog_config.as_ref() {
            let required_info = get_required_info(&changelog_config.changelog);
            for (_package, diff) in &mut packages_diffs {
                for commit in &mut diff.commits {
                    fill_commit(
                        commit,
                        &required_info,
                        repository,
                        &mut all_commits,
                        git_client.as_ref(),
                    )
                    .await
                    .context(
                        "Failed to fetch the commit information required by the changelog template",
                    )?;
                }
            }
        }
        Ok(packages_diffs)
    }

    /// Return the update to apply to the packages that depend on the `initial_changed_packages`.
    ///
    /// ## Args
    ///
    /// - `packages_to_check_for_deps`: The packages that might need to be updated.
    ///   We update them if they depend on any of the `changed_packages`.
    ///   If they don't depend on any of the `changed_packages`, they are not updated
    ///   because they don't contain any new commits.
    /// - `initial_changed_packages`: The packages that have changed (i.e. contains commits).
    fn dependent_packages_update(
        &self,
        packages_to_check_for_deps: &[&Package],
        initial_changed_packages: &[(&Package, Version)],
    ) -> anyhow::Result<PackagesToUpdate> {
        let workspace_manifest = LocalManifest::try_new(self.req.local_manifest())?;
        let workspace_dependencies = workspace_manifest.get_workspace_dependency_table();

        let mut old_changelogs = OldChangelogs::new();
        let workspace_dir = crate::manifest_dir(self.req.local_manifest())?;

        // Track which packages have been processed
        let mut processed: HashSet<String> = initial_changed_packages
            .iter()
            .map(|(p, _)| p.name.to_string())
            .collect();

        let mut result = Vec::new();

        // Keep a copy of all packages that have changed so far
        let mut all_changed_packages: Vec<(&Package, Version)> = initial_changed_packages.to_vec();

        // Continue updating packages until no more dependencies to update are found
        loop {
            let mut any_package_updated = false;

            for p in packages_to_check_for_deps {
                // Skip packages we've already processed in previous iterations
                if processed.contains(p.name.as_ref()) {
                    continue;
                }

                // Check if this package depends on any changed package
                if let Ok(deps) = p.dependencies_to_update(
                    &all_changed_packages,
                    workspace_dependencies,
                    workspace_dir,
                ) && !deps.is_empty()
                {
                    // This package depends on changed packages, so it needs to be updated
                    let update =
                        self.calculate_package_update_result(&deps, p, &mut old_changelogs)?;

                    result.push(update.clone());

                    // Mark as changed so packages depending on it will be updated in the next iteration
                    all_changed_packages.push((p, update.1.version.clone()));
                    processed.insert(p.name.to_string());
                    any_package_updated = true;
                }
            }

            // If no packages were updated in this iteration, we're done
            if !any_package_updated {
                break;
            }
        }

        Ok(result)
    }

    fn calculate_package_update_result(
        &self,
        deps: &[&Package],
        p: &Package,
        old_changelogs: &mut OldChangelogs,
    ) -> anyhow::Result<(Package, UpdateResult)> {
        let deps: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        let commits = {
            let change = format!(
                "chore: updated the following local packages: {}",
                deps.join(", ")
            );
            vec![Commit::new(NO_COMMIT_ID.to_string(), change)]
        };
        let next_version = if p.version.is_prerelease() {
            p.version.increment_prerelease()
        } else {
            p.version.increment_patch()
        };
        info!(
            "{}: dependencies changed. Next version is {next_version}",
            p.name
        );
        let update_result = self.calculate_update_result(
            commits,
            next_version,
            p,
            SemverCheck::Skipped,
            None, // No registry_version for dependency updates
            old_changelogs,
        )?;
        Ok((p.clone(), update_result))
    }

    fn calculate_update_result(
        &self,
        commits: Vec<Commit>,
        next_version: Version,
        p: &Package,
        semver_check: SemverCheck,
        registry_version: Option<Version>,
        old_changelogs: &mut OldChangelogs,
    ) -> Result<UpdateResult, anyhow::Error> {
        let changelog_path = self.req.changelog_path(p);
        let old_changelog: Option<String> = old_changelogs.get_or_read(&changelog_path);
        let update_result = self.update_result(
            commits,
            next_version,
            p,
            semver_check,
            registry_version,
            old_changelog.as_deref(),
        )?;
        if let Some(changelog) = &update_result.changelog {
            old_changelogs.insert(changelog_path, changelog.clone());
        }
        Ok(update_result)
    }

    /// This function needs `old_changelog` so that you can have changes of different
    /// packages in the same changelog.
    fn update_result(
        &self,
        commits: Vec<Commit>,
        version: Version,
        package: &Package,
        semver_check: SemverCheck,
        registry_version: Option<Version>,
        old_changelog: Option<&str>,
    ) -> anyhow::Result<UpdateResult> {
        let repo_url = self.req.repo_url();
        let release_link = {
            // Use registry_version for prev_tag when available (version already bumped case),
            // otherwise use package.version (normal case)
            let prev_version = registry_version
                .as_ref()
                .unwrap_or(&package.version)
                .to_string();
            let prev_tag = self.project.git_tag(&package.name, &prev_version)?;
            let next_tag = self.project.git_tag(&package.name, &version.to_string())?;
            repo_url.map(|r| r.git_release_link(&prev_tag, &next_tag))
        };

        let changelog_outcome = {
            let cfg = self.req.get_package_config(package.name.as_str());
            let changelog_req = cfg
                .should_update_changelog()
                .then_some(self.req.changelog_req().clone());
            let commits: Vec<Commit> = commits
                .into_iter()
                // If not conventional commit, only consider the first line of the commit message.
                .filter_map(|c| {
                    if c.is_conventional() {
                        Some(c)
                    } else {
                        c.message.lines().next().map(|line| Commit {
                            message: line.to_string(),
                            ..c
                        })
                    }
                })
                .collect();
            changelog_req
                .map(|r| {
                    get_changelog(
                        &commits,
                        &version,
                        Some(r),
                        old_changelog,
                        repo_url,
                        release_link.as_deref(),
                        package,
                    )
                })
                .transpose()
        }?;

        let (changelog, new_changelog_entry) = match changelog_outcome {
            Some((changelog, new_changelog_entry)) => (Some(changelog), Some(new_changelog_entry)),
            None => (None, None),
        };

        Ok(UpdateResult {
            version,
            changelog,
            semver_check,
            new_changelog_entry,
            registry_version,
        })
    }

    /// This operation is not thread-safe, because we do `git checkout` on the repository.
    #[instrument(
        skip_all,
        fields(package = %package.name)
    )]
    fn get_diff(
        &self,
        package: &Package,
        registry_packages: &PackagesCollection,
        repository: &Repo,
    ) -> anyhow::Result<Diff> {
        info!(
            "determining next version for {} {}",
            package.name, package.version
        );
        let package_path = get_package_path(package, repository, self.project.root())
            .context("failed to determine package path")?;

        repository
            .checkout_head()
            .context("can't checkout head to calculate diff")?;
        let registry_package = registry_packages.get_registry_package(&package.name);
        let mut diff = Diff::new(registry_package.is_some());
        let pathbufs_to_check = pathbufs_to_check(&package_path, package)?;
        let paths_to_check: Vec<&Path> = pathbufs_to_check.iter().map(|p| p.as_ref()).collect();
        repository
            .checkout_last_commit_at_paths(&paths_to_check)
            .map_err(|err| {
                if err
                    .to_string()
                    .contains("Your local changes to the following files would be overwritten")
                {
                    err.context("The allow-dirty option can't be used in this case")
                } else {
                    err.context("Failed to retrieve the last commit of local repository.")
                }
            })?;

        // Always diff from the last stable (non-RC) tag. This ensures that
        // when the current version is e.g. 1.2.0-rc.1, we diff from the last
        // stable tag (1.2.0) rather than the RC tag.
        let (git_tag, tag_commit) = if let Some(stable_info) =
            find_last_stable_tag(self.project, &package.name, repository)
        {
            let tag = self
                .project
                .git_tag(&package.name, &stable_info.version.to_string())?;
            diff.base_version = Some(stable_info.version);
            (tag, Some(stable_info.commit))
        } else {
            // No stable tag found — fall back to constructing tag from package version.
            let tag = self
                .project
                .git_tag(&package.name, &package.version.to_string())?;
            let commit = repository.get_tag_commit(&tag);
            (tag, commit)
        };

        info!(
            "{}: diffing from tag {} (commit {})",
            package.name,
            git_tag,
            tag_commit.as_deref().unwrap_or("none")
        );

        // Check if git_only is enabled for this package
        let using_git_only = || self.req.should_use_git_only(&package.name);

        if tag_commit.is_some() && !using_git_only() {
            // Only check registry for packages that should be published
            // Skip this check if git_only is enabled (we don't use registry in that mode)
            let config = self.req.get_package_config(&package.name);
            if config.should_publish() {
                let registry_package = registry_package.with_context(|| format!("package `{}` not found in the registry, but the git tag {git_tag} exists. Consider running `cargo publish` manually to publish this package.", package.name))?;
                anyhow::ensure!(
                    package.version <= registry_package.package.version,
                    "local package `{}` has a greater version ({}) with respect to the registry package ({}), but the git tag {git_tag} exists. Consider running `cargo publish` manually to publish the new version of this package.",
                    package.name,
                    package.version,
                    registry_package.package.version
                );
            }
        }
        self.get_package_diff(
            &package_path,
            package,
            registry_package,
            repository,
            tag_commit.as_deref(),
            &mut diff,
        )?;

        repository
            .checkout_head()
            .context("can't checkout to head after calculating diff")?;
        Ok(diff)
    }

    fn get_package_diff(
        &self,
        package_path: &Utf8Path,
        package: &Package,
        registry_package: Option<&RegistryPackage>,
        repository: &Repo,
        tag_commit: Option<&str>,
        diff: &mut Diff,
    ) -> anyhow::Result<()> {
        let pathbufs_to_check = pathbufs_to_check(package_path, package)?;
        let paths_to_check: Vec<&Path> = pathbufs_to_check.iter().map(|p| p.as_ref()).collect();
        let max_analyze_commits = if registry_package.is_none() {
            match self.req.max_analyze_commits() {
                0 => u32::MAX,
                n => n,
            }
        } else {
            u32::MAX
        };

        for _ in 0..max_analyze_commits {
            let current_commit_message = repository.current_commit_message()?;
            let current_commit_hash = repository.current_commit_hash()?;

            // Check if files changed in git commit belong to the current package.
            // This is required because a package can contain another package in a subdirectory.
            let are_changed_files_in_pkg = || {
                self.are_changed_files_in_package(package_path, repository, &current_commit_hash)
            };

            if let Some(registry_package) = registry_package {
                debug!(
                    "package {} found in cargo registry",
                    registry_package.package.name
                );
                let registry_package_path = registry_package.package.package_path()?;

                let are_packages_equal = self.check_package_equality(
                    repository,
                    package,
                    package_path,
                    registry_package_path,
                ).with_context(|| format!("failed to check package equality for `{}` at commit {current_commit_hash}", package.name))?;
                let commit_too_old = || {
                    is_commit_too_old(
                        repository,
                        tag_commit,
                        registry_package.published_at_sha1(),
                        &current_commit_hash,
                    )
                };
                if are_packages_equal || commit_too_old() {
                    debug!(
                        "next version calculated starting from commits after `{current_commit_hash}`"
                    );
                    if diff.commits.is_empty() {
                        // Even if the packages are equal, the Cargo.lock or Cargo.toml of the
                        // workspace might have changed.
                        // If the dependencies changed, we add a commit to the diff.
                        self.add_dependencies_update_if_any(
                            diff,
                            &registry_package.package,
                            package,
                            registry_package_path,
                        )?;
                    }
                    // The local package is identical to the registry one, which means that
                    // the package was published at this commit, so we will not count this commit
                    // as part of the release.
                    // We can process the next package.
                    break;
                } else {
                    // When version is already bumped, we still collect commits to update the changelog,
                    // but mark that version should not be bumped further.
                    if package.version > registry_package.package.version
                        && diff.is_version_published
                    {
                        info!(
                            "{}: local version ({}) > registry version ({}). Only changelog will be updated.",
                            package.name, package.version, registry_package.package.version
                        );
                        diff.set_version_unpublished(registry_package.package.version.clone());
                    }
                    if are_changed_files_in_pkg()? {
                        // At this point of the git history, the two packages are different,
                        // which means that this commit is not present in the published package.
                        let first_line = current_commit_message.lines().next().unwrap_or("");
                        info!(
                            "{}: commit {} {}",
                            package.name,
                            &current_commit_hash[..7.min(current_commit_hash.len())],
                            first_line
                        );
                        diff.commits.push(Commit::new(
                            current_commit_hash,
                            current_commit_message.clone(),
                        ));
                    }
                }
            } else if are_changed_files_in_pkg()? {
                let first_line = current_commit_message.lines().next().unwrap_or("");
                info!(
                    "{}: commit {} {}",
                    package.name,
                    &current_commit_hash[..7.min(current_commit_hash.len())],
                    first_line
                );
                diff.commits.push(Commit::new(
                    current_commit_hash,
                    current_commit_message.clone(),
                ));
            }
            // Go back to the previous commit.
            // Keep in mind that the info contained in `package` might be outdated,
            // because commits could contain changes to Cargo.toml.
            if let Err(_err) = repository.checkout_previous_commit_at_paths(&paths_to_check) {
                debug!("there are no other commits");
                break;
            }
        }
        Ok(())
    }

    fn check_package_equality(
        &self,
        repository: &Repo,
        package: &Package,
        package_path: &Utf8Path,
        registry_package_path: &Utf8Path,
    ) -> anyhow::Result<bool> {
        if crate::is_readme_updated(&package.name, package_path, registry_package_path)? {
            debug!("{}: README updated", package.name);
            return Ok(false);
        }
        // We run `cargo package` when comparing packages, which can edit files, such as `Cargo.lock`.
        // Store its path so it can be reverted after comparison.
        let cargo_lock_path = self
            .get_cargo_lock_path(repository)
            .context("failed to determine Cargo.lock path")?;
        let are_packages_equal = crate::are_packages_equal(package_path, registry_package_path)
            .context("cannot compare packages")?;
        if let Some(cargo_lock_path) = cargo_lock_path.as_deref() {
            // Revert any changes to `Cargo.lock`
            repository
                .checkout(cargo_lock_path)
                .context("cannot revert changes introduced when comparing packages")?;
        }
        Ok(are_packages_equal)
    }

    /// If the dependencies changed, add a commit to the diff.
    fn add_dependencies_update_if_any(
        &self,
        diff: &mut Diff,
        registry_package: &Package,
        package: &Package,
        registry_package_path: &Utf8Path,
    ) -> anyhow::Result<()> {
        let are_toml_dependencies_updated = || {
            toml_compare::are_toml_dependencies_updated(
                &registry_package.dependencies,
                &package.dependencies,
            )
        };
        let are_lock_dependencies_updated = || {
            lock_compare::are_lock_dependencies_updated(
                &self.project.cargo_lock_path(),
                registry_package_path,
            )
            .context("Can't check if Cargo.lock dependencies are up to date")
        };
        if are_toml_dependencies_updated() {
            diff.commits.push(Commit::new(
                NO_COMMIT_ID.to_string(),
                "chore: update Cargo.toml dependencies".to_string(),
            ));
        } else if contains_executable(package) && are_lock_dependencies_updated()? {
            diff.commits.push(Commit::new(
                NO_COMMIT_ID.to_string(),
                "chore: update Cargo.lock dependencies".to_string(),
            ));
        } else {
            info!("{}: already up to date", package.name);
        }
        Ok(())
    }

    fn get_cargo_lock_path(&self, repository: &Repo) -> anyhow::Result<Option<String>> {
        let project_cargo_lock = self.project.cargo_lock_path();
        let relative_lock_path = fs_utils::strip_prefix(&project_cargo_lock, self.project.root())?;
        let repository_cargo_lock = repository.directory().join(relative_lock_path);
        if repository_cargo_lock.exists() {
            Ok(Some(repository_cargo_lock.to_string()))
        } else {
            Ok(None)
        }
    }

    fn get_next_version(
        &self,
        new_workspace_version: Option<&Version>,
        p: &Package,
        workspace_version_pkgs: &HashSet<String>,
        version_groups: &HashMap<String, Version>,
        diff: &Diff,
    ) -> anyhow::Result<Version> {
        let pkg_config = self.req.get_package_config(&p.name);
        let next_version = match new_workspace_version {
            Some(max_workspace_version) if workspace_version_pkgs.contains(p.name.as_str()) => {
                debug!(
                    "next version of {} is workspace version: {max_workspace_version}",
                    p.name
                );
                max_workspace_version.clone()
            }
            _ => {
                if let Some(version_group) = pkg_config.version_group {
                    version_groups
                        .get(&version_group)
                        .with_context(|| {
                            format!("failed to retrieve version for version group {version_group}")
                        })?
                        .clone()
                } else {
                    let version_updater = pkg_config.generic.version_updater()?;
                    // When a stable base_version is available (e.g. the current
                    // Cargo.toml version is an RC like 1.2.0-rc.1 but the last
                    // stable tag is 1.2.0), compute the next version from the
                    // stable base so that conventional-commit analysis isn't
                    // short-circuited by the prerelease check.
                    match &diff.base_version {
                        Some(base) => base.next_from_diff(diff, version_updater),
                        None => p.version.next_from_diff(diff, version_updater),
                    }
                }
            }
        };
        Ok(next_version)
    }

    /// `hash` is only used for logging purposes.
    fn are_changed_files_in_package(
        &self,
        package_path: &Utf8Path,
        repository: &Repo,
        hash: &str,
    ) -> anyhow::Result<bool> {
        // We run `cargo package` to get package files, which can edit files, such as `Cargo.lock`.
        // Store its path so it can be reverted after comparison.
        let cargo_lock_path = self
            .get_cargo_lock_path(repository)
            .context("failed to determine Cargo.lock path")?;
        let package_files_res = get_package_files(package_path, repository);
        if let Some(cargo_lock_path) = cargo_lock_path.as_deref() {
            // Revert any changes to `Cargo.lock`
            repository
                .checkout(cargo_lock_path)
                .context("cannot revert changes introduced when comparing packages")?;
        }
        let Ok(package_files) = package_files_res.inspect_err(|e| {
            info!("failed to get package files at commit {hash}: {e:?}");
        }) else {
            return Ok(false);
        };
        let Ok(changed_files) = repository.files_of_current_commit().inspect_err(|e| {
            warn!("failed to get changed files of commit {hash}: {e:?}");
        }) else {
            // Assume that this commit contains changes to the package.
            return Ok(true);
        };
        Ok(!package_files.is_disjoint(&changed_files))
    }
}

/// Check if release-plz should check the semver compatibility of the package.
/// - `run_semver_check` is true if the user wants to run the semver check.
fn should_check_semver(package: &Package, run_semver_check: bool) -> bool {
    if run_semver_check && contains_library(package) {
        let is_cargo_semver_checks_installed = semver_check::is_cargo_semver_checks_installed();
        if !is_cargo_semver_checks_installed {
            warn!(
                "cargo-semver-checks not installed, skipping semver check. For more information, see https://release-plz.dev/docs/semver-check"
            );
        }
        return is_cargo_semver_checks_installed;
    }
    false
}

fn contains_executable(package: &Package) -> bool {
    contains_target_kind(package, &TargetKind::Bin)
}

fn contains_library(package: &Package) -> bool {
    contains_target_kind(package, &TargetKind::Lib)
}

fn contains_target_kind(package: &Package, target_kind: &TargetKind) -> bool {
    // We use target `kind` because target `crate_types` contains "Bin" if the kind is "Test".
    package.targets.iter().any(|t| t.kind.contains(target_kind))
}

/// Get files that belong to the package.
/// The paths are relative to the git repo root.
fn get_package_files(
    package_path: &Utf8Path,
    repository: &Repo,
) -> anyhow::Result<HashSet<Utf8PathBuf>> {
    // Get relative path of the crate with respect to the repository because we need to compare
    // files with the git output.
    // Canonicalize repository_dir so it matches the canonicalized file paths below.
    // Without this, symlinks in the temp directory path (e.g. /tmp -> /private/tmp on macOS)
    // cause strip_prefix to fail.
    let raw_repo_dir = repository.directory();
    let repository_dir = fs_utils::canonicalize_utf8(raw_repo_dir)
        .with_context(|| format!("failed to canonicalize repository dir {raw_repo_dir}"))?;

    let files = crate::get_cargo_package_files(package_path)
        .with_context(|| format!("cargo package --list failed at {package_path}"))?;

    info!(
        "get_package_files: repo_dir={repository_dir}, package_path={package_path}, file_count={}",
        files.len()
    );

    files
        .into_iter()
        // filter files generated by `cargo package` that aren't in git.
        .filter(|file| file != "Cargo.toml.orig" && file != ".cargo_vcs_info.json")
        .filter_map(|file| {
            let file_path = package_path.join(&file);
            // Skip files that don't exist at the package path (e.g. Cargo.lock
            // which cargo includes from the workspace root, not the package dir).
            if !file_path.exists() {
                return None;
            }
            let result = (|| {
                let normalized = fs_utils::canonicalize_utf8(&file_path)?;
                let relative_path = normalized
                    .strip_prefix(&repository_dir)
                    .with_context(|| {
                        format!("failed to strip {repository_dir} from {normalized}")
                    })?;
                Ok(relative_path.to_path_buf())
            })();
            Some(result)
        })
        .collect()
}

/// Check if commit belongs to a previous version of the package.
/// `tag_commit` is the commit hash of the tag of the previous version.
/// `published_at_commit` is the commit hash where `cargo publish` ran.
fn is_commit_too_old(
    repository: &Repo,
    tag_commit: Option<&str>,
    published_at_commit: Option<&str>,
    current_commit_hash: &str,
) -> bool {
    if let Some(tag_commit) = tag_commit.as_ref()
        && repository.is_ancestor(current_commit_hash, tag_commit)
    {
        debug!(
            "stopping looking at git history because the current commit ({}) is an ancestor of the commit ({}) tagged with the previous version.",
            current_commit_hash, tag_commit
        );
        return true;
    }

    if let Some(published_commit) = published_at_commit.as_ref()
        && repository.is_ancestor(current_commit_hash, published_commit)
    {
        debug!(
            "stopping looking at git history because the current commit ({}) is an ancestor of the commit ({}) where the previous version was published.",
            current_commit_hash, published_commit
        );
        return true;
    }
    false
}

fn pathbufs_to_check(
    package_path: &Utf8Path,
    package: &Package,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let mut paths = vec![package_path.to_path_buf()];
    if let Some(readme_path) = crate::local_readme_override(package, package_path)? {
        paths.push(readme_path);
    }
    Ok(paths)
}

/// Return the following tuple:
/// - the entire changelog (with the new entries);
/// - the new changelog entry alone
///   (i.e. changelog body update without header and footer).
fn get_changelog(
    commits: &[Commit],
    next_version: &Version,
    changelog_req: Option<ChangelogRequest>,
    old_changelog: Option<&str>,
    repo_url: Option<&RepoUrl>,
    release_link: Option<&str>,
    package: &Package,
) -> anyhow::Result<(String, String)> {
    let commits: Vec<git_cliff_core::commit::Commit> =
        commits.iter().map(|c| c.to_cliff_commit()).collect();
    let mut changelog_builder = ChangelogBuilder::new(
        commits.clone(),
        next_version.to_string(),
        package.name.to_string(),
    );
    if let Some(changelog_req) = changelog_req {
        if let Some(release_date) = changelog_req.release_date {
            changelog_builder = changelog_builder.with_release_date(release_date);
        }
        if let Some(config) = changelog_req.changelog_config {
            changelog_builder = changelog_builder.with_config(config);
        }
        if let Some(link) = release_link {
            changelog_builder = changelog_builder.with_release_link(link);
        }
        if let Some(repo_url) = repo_url {
            let remote = Remote {
                owner: repo_url.owner.clone(),
                repo: repo_url.name.clone(),
                link: repo_url.full_host(),
                contributors: get_contributors(&commits),
            };
            changelog_builder = changelog_builder.with_remote(remote);

            let pr_link = repo_url.git_pr_link();
            changelog_builder = changelog_builder.with_pr_link(pr_link);
        }
        let is_package_published = next_version != &package.version;

        let last_version = old_changelog.and_then(|old_changelog| {
            changelog_parser::last_version_from_str(old_changelog)
                .ok()
                .flatten()
        });
        if is_package_published {
            let last_version = last_version.unwrap_or(package.version.to_string());
            changelog_builder = changelog_builder.with_previous_version(last_version);
        } else if let Some(last_version) = last_version
            && let Some(old_changelog) = old_changelog
            && last_version == next_version.to_string()
        {
            // If the next version is the same as the last version of the changelog,
            // don't update the changelog (returning the old one).
            // This can happen when no version of the package was published,
            // but the changelog already contains the changes of the initial version
            // of the package (e.g. because a release PR was merged).
            return Ok((old_changelog.to_string(), String::new()));
        }
    }
    let new_changelog = changelog_builder.build();
    let changelog = match old_changelog {
        Some(old_changelog) => new_changelog.prepend(old_changelog)?,
        None => new_changelog.generate()?, // Old changelog doesn't exist.
    };
    let body_only =
        new_changelog_entry(changelog_builder).context("can't determine changelog body")?;
    Ok((changelog, body_only.unwrap_or_default()))
}

fn new_changelog_entry(changelog_builder: ChangelogBuilder) -> anyhow::Result<Option<String>> {
    changelog_builder
        .config()
        .cloned()
        .map(|c| {
            let new_config = Config {
                changelog: ChangelogConfig {
                    // If we set None, later this will be overriden with the defaults.
                    // Instead we just want the body.
                    header: Some(String::new()),
                    footer: Some(String::new()),
                    ..c.changelog
                },
                ..c
            };
            let changelog = changelog_builder.with_config(new_config).build();
            changelog.generate().map(|entry| entry.trim().to_string())
        })
        .transpose()
}

fn get_contributors(commits: &[git_cliff_core::commit::Commit]) -> Vec<RemoteContributor> {
    let mut unique_contributors = HashSet::new();
    commits
        .iter()
        .filter_map(|c| c.remote.clone())
        // Filter out duplicate contributors.
        // `insert` returns false if the contributor is already in the set.
        .filter(|remote| unique_contributors.insert(remote.username.clone()))
        .collect()
}

fn get_package_path(
    package: &Package,
    repository: &Repo,
    project_root: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    let package_path = package.package_path()?;
    get_repo_path(package_path, repository, project_root)
}

fn get_repo_path(
    old_path: &Utf8Path,
    repository: &Repo,
    project_root: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    let relative_path = fs_utils::strip_prefix(old_path, project_root)
        .context("error while retrieving package_path")?;
    let result_path = repository.directory().join(relative_path);

    Ok(result_path)
}

/// Information about the last stable (non-RC) tag for a package.
struct StableTagInfo {
    /// The stable version parsed from the tag.
    version: Version,
    /// The commit hash pointed to by the tag.
    commit: String,
}

/// Parse a semver version from a tag in the form `{prefix}{version}{suffix}`.
fn parse_version_from_tag(tag: &str, prefix: &str, suffix: &str) -> Option<Version> {
    tag.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .and_then(|v| Version::parse(v).ok())
}

/// Given all tags and the prefix/suffix pattern from the tag template,
/// find the highest stable (non-prerelease) version among tags accepted by `tag_filter`.
fn best_stable_version_from_tags<F>(
    tags: &[String],
    prefix: &str,
    suffix: &str,
    mut tag_filter: F,
) -> Option<Version>
where
    F: FnMut(&str) -> bool,
{
    tags.iter()
        .map(String::as_str)
        .filter(|tag| tag_filter(tag))
        .filter_map(|tag| parse_version_from_tag(tag, prefix, suffix))
        .filter(|v| v.pre.is_empty())
        .max()
}

/// Find the last stable (non-prerelease) tag for a package.
///
/// Scans all tags in the repository, matches them against the package's tag template,
/// extracts versions, and returns the highest one that has no prerelease component.
///
/// Returns `None` if no stable tag is found.
fn find_last_stable_tag(
    project: &Project,
    package_name: &str,
    repository: &Repo,
) -> Option<StableTagInfo> {
    let all_tags = repository.get_all_tags();
    if all_tags.is_empty() {
        return None;
    }

    // Generate a tag with a known placeholder version to determine the prefix/suffix pattern.
    let placeholder = "0.0.0-placeholder";
    let Ok(rendered) = project.git_tag(package_name, placeholder) else {
        return None;
    };

    let (prefix, suffix) = rendered.split_once(placeholder)?;

    // Only consider tags reachable from the current checkout commit.
    // This avoids selecting tags from unrelated branches/release lines.
    let current_commit = repository.current_commit_hash().ok()?;
    let version = best_stable_version_from_tags(&all_tags, prefix, suffix, |tag| {
        let Some(tag_commit) = repository.get_tag_commit(tag) else {
            return false;
        };
        repository.is_ancestor(&tag_commit, &current_commit)
    })?;
    let tag = project.git_tag(package_name, &version.to_string()).ok()?;
    let commit = repository.get_tag_commit(&tag)?;
    Some(StableTagInfo { version, commit })
}

/// Parse an RC number from a tag in the form `{prefix}{rc_number}{suffix}`.
fn parse_rc_number_from_tag(tag: &str, prefix: &str, suffix: &str) -> Option<u64> {
    tag.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .and_then(|n| n.parse::<u64>().ok())
}

/// Given all tags and a rendered RC prefix/suffix pair,
/// find the highest existing RC number among tags accepted by `tag_filter`.
/// Returns 0 if none found.
fn max_rc_number_from_tags<F>(
    tags: &[String],
    rc_tag_prefix: &str,
    rc_tag_suffix: &str,
    mut tag_filter: F,
) -> u64
where
    F: FnMut(&str) -> bool,
{
    tags.iter()
        .map(String::as_str)
        .filter(|tag| tag_filter(tag))
        .filter_map(|tag| parse_rc_number_from_tag(tag, rc_tag_prefix, rc_tag_suffix))
        .max()
        .unwrap_or(0)
}

/// Find the next RC number for a given package and target stable version.
///
/// Scans all tags for patterns like `pkg-v{target}-rc.N` and returns `max(N) + 1`.
/// Returns 1 if no existing RC tags are found.
fn find_next_rc_number(
    project: &Project,
    package_name: &str,
    target_stable: &Version,
    repository: &Repo,
) -> u64 {
    let all_tags = repository.get_all_tags();
    let current_commit = repository.current_commit_hash().ok();

    // Build the expected tag shape for this target version's RCs.
    // E.g. for target 1.1.0 and template "{{package}}-v{{version}}",
    // rendered is "pkg-v1.1.0-rc.__RCNUM__".
    let rc_placeholder = "__RCNUM__";
    let rc_version_with_placeholder = format!("{target_stable}-rc.{rc_placeholder}");
    let Ok(rendered) = project.git_tag(package_name, &rc_version_with_placeholder) else {
        return 1;
    };
    let Some((prefix, suffix)) = rendered.split_once(rc_placeholder) else {
        return 1;
    };

    let max_rc = max_rc_number_from_tags(&all_tags, prefix, suffix, |tag| {
        // Keep behavior robust if current commit cannot be determined.
        let Some(current_commit) = current_commit.as_ref() else {
            return true;
        };
        let Some(tag_commit) = repository.get_tag_commit(tag) else {
            return false;
        };
        repository.is_ancestor(&tag_commit, current_commit)
    });

    max_rc + 1
}

/// Propagate bump levels through the dependency graph.
///
/// `entries`: `(package_name, own_bump, dependency_names)` for each package in the workspace.
/// `ordered_names`: topologically sorted package names (leaves first).
///
/// Returns a map from package name to final (propagated) bump level.
fn propagate_bumps(
    entries: &[(String, BumpLevel, Vec<String>)],
    ordered_names: &[String],
) -> HashMap<String, BumpLevel> {
    let name_to_bump: HashMap<&str, BumpLevel> = entries
        .iter()
        .map(|(name, bump, _)| (name.as_str(), *bump))
        .collect();
    let name_to_deps: HashMap<&str, &Vec<String>> = entries
        .iter()
        .map(|(name, _, deps)| (name.as_str(), deps))
        .collect();

    let mut final_bumps: HashMap<String, BumpLevel> = HashMap::new();

    for pkg_name in ordered_names {
        if let Some(&own_bump) = name_to_bump.get(pkg_name.as_str()) {
            let max_dep_bump = name_to_deps
                .get(pkg_name.as_str())
                .into_iter()
                .flat_map(|deps| deps.iter())
                .filter_map(|d| final_bumps.get(d.as_str()))
                .copied()
                .max()
                .unwrap_or(BumpLevel::None);

            final_bumps.insert(pkg_name.clone(), own_bump.max(max_dep_bump));
        }
    }

    final_bumps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_version_is_not_added_to_changelog() {
        let commits = vec![
            Commit::new(crate::NO_COMMIT_ID.to_string(), "fix: myfix".to_string()),
            Commit::new(crate::NO_COMMIT_ID.to_string(), "simple update".to_string()),
        ];

        let next_version = Version::new(1, 1, 0);
        let changelog_req = ChangelogRequest::default();

        let old = r"## [1.1.0] - 1970-01-01

### fix bugs
- my awesomefix

### other
- complex update
";
        let new = get_changelog(
            &commits,
            &next_version,
            Some(changelog_req),
            Some(old),
            None,
            None,
            &fake_package::FakePackage::new("my_package").into(),
        )
        .unwrap();
        assert_eq!(old, new.0);
    }

    // ── best_stable_version_from_tags tests ─────────────────────────

    fn tags(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stable_tag_found_among_rc_tags() {
        let t = tags(&["pkg-v1.0.0", "pkg-v1.1.0-rc.1", "pkg-v1.1.0-rc.2"]);
        assert_eq!(
            best_stable_version_from_tags(&t, "pkg-v", "", |_| true),
            Some(Version::new(1, 0, 0))
        );
    }

    #[test]
    fn highest_stable_selected() {
        let t = tags(&["pkg-v1.0.0", "pkg-v1.1.0", "pkg-v0.9.0"]);
        assert_eq!(
            best_stable_version_from_tags(&t, "pkg-v", "", |_| true),
            Some(Version::new(1, 1, 0))
        );
    }

    #[test]
    fn no_stable_tags_returns_none() {
        let t = tags(&["pkg-v1.0.0-rc.1", "pkg-v1.0.0-rc.2"]);
        assert_eq!(
            best_stable_version_from_tags(&t, "pkg-v", "", |_| true),
            None
        );
    }

    #[test]
    fn empty_tags_returns_none() {
        assert_eq!(
            best_stable_version_from_tags(&[], "pkg-v", "", |_| true),
            None
        );
    }

    #[test]
    fn non_matching_tags_ignored() {
        let t = tags(&["other-v1.0.0", "pkg-v0.5.0", "unrelated"]);
        assert_eq!(
            best_stable_version_from_tags(&t, "pkg-v", "", |_| true),
            Some(Version::new(0, 5, 0))
        );
    }

    #[test]
    fn non_ancestor_stable_tags_are_ignored() {
        let t = tags(&["pkg-v1.0.0", "pkg-v2.0.0"]);
        assert_eq!(
            best_stable_version_from_tags(&t, "pkg-v", "", |tag| tag != "pkg-v2.0.0"),
            Some(Version::new(1, 0, 0))
        );
    }

    // ── max_rc_number_from_tags tests ───────────────────────────────

    #[test]
    fn finds_max_rc() {
        let t = tags(&["pkg-v1.1.0-rc.1", "pkg-v1.1.0-rc.3", "pkg-v1.1.0-rc.2"]);
        assert_eq!(
            max_rc_number_from_tags(&t, "pkg-v1.1.0-rc.", "", |_| true),
            3
        );
    }

    #[test]
    fn no_rc_tags_returns_zero() {
        assert_eq!(
            max_rc_number_from_tags(&[], "pkg-v1.1.0-rc.", "", |_| true),
            0
        );
    }

    #[test]
    fn different_version_rc_ignored() {
        let t = tags(&["pkg-v1.0.0-rc.5", "pkg-v1.0.0-rc.3"]);
        // Looking for 1.1.0 RCs, but only 1.0.0 RCs exist.
        assert_eq!(
            max_rc_number_from_tags(&t, "pkg-v1.1.0-rc.", "", |_| true),
            0
        );
    }

    #[test]
    fn non_numeric_suffix_ignored() {
        let t = tags(&["pkg-v1.1.0-rc.2", "pkg-v1.1.0-rc.beta", "pkg-v1.1.0-rc.3"]);
        assert_eq!(
            max_rc_number_from_tags(&t, "pkg-v1.1.0-rc.", "", |_| true),
            3
        );
    }

    #[test]
    fn suffix_templates_are_supported_for_rc_numbers() {
        let t = tags(&[
            "release-pkg-1.1.0-rc.2-prod",
            "release-pkg-1.1.0-rc.10-prod",
            "release-pkg-1.1.0-prod",
        ]);
        assert_eq!(
            max_rc_number_from_tags(&t, "release-pkg-1.1.0-rc.", "-prod", |_| true),
            10
        );
    }

    #[test]
    fn non_ancestor_rc_tags_are_ignored() {
        let t = tags(&["pkg-v1.1.0-rc.2", "pkg-v1.1.0-rc.9", "pkg-v1.1.0-rc.3"]);
        assert_eq!(
            max_rc_number_from_tags(&t, "pkg-v1.1.0-rc.", "", |tag| tag != "pkg-v1.1.0-rc.9"),
            3
        );
    }

    // ── propagate_bumps tests ───────────────────────────────────────

    #[test]
    fn dep_bump_propagates_to_parent() {
        // B has major bump, A depends on B with no own bump → A gets major.
        let entries = vec![
            ("b".into(), BumpLevel::Major, vec![]),
            ("a".into(), BumpLevel::None, vec!["b".into()]),
        ];
        let order = vec!["b".into(), "a".into()];
        let result = propagate_bumps(&entries, &order);
        assert_eq!(result["a"], BumpLevel::Major);
        assert_eq!(result["b"], BumpLevel::Major);
    }

    #[test]
    fn own_bump_wins_when_higher() {
        // A has major, depends on B with patch → A stays major.
        let entries = vec![
            ("b".into(), BumpLevel::Patch, vec![]),
            ("a".into(), BumpLevel::Major, vec!["b".into()]),
        ];
        let order = vec!["b".into(), "a".into()];
        let result = propagate_bumps(&entries, &order);
        assert_eq!(result["a"], BumpLevel::Major);
        assert_eq!(result["b"], BumpLevel::Patch);
    }

    #[test]
    fn transitive_chain() {
        // C has minor, B depends on C (no own bump), A depends on B (no own bump).
        // All should get minor through transitive propagation.
        let entries = vec![
            ("c".into(), BumpLevel::Minor, vec![]),
            ("b".into(), BumpLevel::None, vec!["c".into()]),
            ("a".into(), BumpLevel::None, vec!["b".into()]),
        ];
        let order = vec!["c".into(), "b".into(), "a".into()];
        let result = propagate_bumps(&entries, &order);
        assert_eq!(result["c"], BumpLevel::Minor);
        assert_eq!(result["b"], BumpLevel::Minor);
        assert_eq!(result["a"], BumpLevel::Minor);
    }

    #[test]
    fn no_deps_preserves_own_bump() {
        let entries = vec![("a".into(), BumpLevel::Patch, vec![])];
        let order = vec!["a".into()];
        let result = propagate_bumps(&entries, &order);
        assert_eq!(result["a"], BumpLevel::Patch);
    }

    #[test]
    fn multiple_deps_takes_max() {
        // A depends on B(patch) and C(major) → A gets major.
        let entries = vec![
            ("b".into(), BumpLevel::Patch, vec![]),
            ("c".into(), BumpLevel::Major, vec![]),
            ("a".into(), BumpLevel::None, vec!["b".into(), "c".into()]),
        ];
        let order = vec!["b".into(), "c".into(), "a".into()];
        let result = propagate_bumps(&entries, &order);
        assert_eq!(result["a"], BumpLevel::Major);
    }

    #[test]
    fn all_none_stays_none() {
        let entries = vec![
            ("b".into(), BumpLevel::None, vec![]),
            ("a".into(), BumpLevel::None, vec!["b".into()]),
        ];
        let order = vec!["b".into(), "a".into()];
        let result = propagate_bumps(&entries, &order);
        assert_eq!(result["a"], BumpLevel::None);
        assert_eq!(result["b"], BumpLevel::None);
    }
}
