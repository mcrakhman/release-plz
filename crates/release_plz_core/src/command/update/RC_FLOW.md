# RC Flow: Code Walkthrough

## The Problem

release-plz calculates version bumps per-package by finding the git tag that matches the package's current `Cargo.toml` version, then walking commits from that tag forward. Two things break when a package is at an RC version like `1.2.0-rc.1`:

### Problem 1 — Wrong diff baseline

`get_diff()` constructs a tag from `package.version` (e.g., `pkg-v1.2.0-rc.1`) and diffs from there. That means you only see commits *since the RC was tagged*, not since the last stable release `1.2.0`. In a multi-package workspace where packages release independently, some packages may still be at their stable version while others are at an RC — each needs to diff from its own last stable tag.

### Problem 2 — Prerelease short-circuit in `VersionIncrement`

Even if you fix the baseline, the existing `next_from_diff()` path calls `VersionUpdater::increment()` → `VersionIncrement::from_commits_with_updater()`. At `version_increment.rs:70-71`:

```rust
if !current_version.pre.is_empty() {
    return Some(Self::Prerelease);
}
```

If you pass `1.2.0-rc.1` as the `current_version`, it short-circuits — every commit becomes a prerelease bump to `1.2.0-rc.2`, regardless of whether commits were `feat!:` (major) or `fix:` (patch). Conventional commit analysis is completely skipped.

### Problem 3 — Dependency bumps are always patch

In the existing `dependent_packages_update`, when package B changes and package A depends on B, A gets an unconditional `increment_patch()`. If B had a *major* breaking change, A should also get at least a major bump — the bump level should propagate.

## What `package.version` Actually Is

`package.version` is the version **currently written in the local `Cargo.toml`**. It is NOT the registry version.

### How it gets populated

1. `Updater.project` holds `Project.packages` — a `Vec<Package>` from `cargo_metadata`.
2. `Project::new()` calls `workspace_packages(metadata)` which calls `cargo_utils::workspace_members(metadata)`.
3. `metadata` comes from `cargo_metadata::MetadataCommand::new().no_deps().manifest_path(path).exec()` — this runs `cargo metadata` which parses the **local** Cargo.toml files.

So if someone has manually set `version = "1.2.0-rc.1"` in their Cargo.toml (or release-plz previously bumped it to that), `package.version` is `1.2.0-rc.1`.

### Where the registry version comes from (separately)

The registry version comes through a completely different path:

- `registry_packages` is a `PackagesCollection`, populated by `collect_registry_packages()` in `next_ver.rs`.
- That calls `get_registry_packages()` in `registry_packages.rs`, which **downloads** the published package from crates.io (or a custom registry).
- It reads the *downloaded* package's Cargo.toml via `cargo_metadata` to get `registry_package.package.version`.

So there are two separate `Package` objects for the same crate:
- `package` (or `p`) — local, from the working tree's Cargo.toml.
- `registry_package.package` — from the last published version on the registry.

### Where the two are compared

In `get_package_diff()` at `updater.rs:898-905`:

```rust
if package.version > registry_package.package.version
    && diff.is_version_published
{
    diff.set_version_unpublished(registry_package.package.version.clone());
}
```

This detects the "version already bumped" case — when someone (or a previous release-plz run) has already edited `Cargo.toml` to a higher version than what's published. When this triggers, `is_version_published` is set to `false`, which makes `should_update_version()` return `false`, meaning release-plz won't bump the version further — it only updates the changelog.

## How the Changes Solve Each Problem

### 1. `find_last_stable_tag()` — Finding the right baseline (`updater.rs:1317-1361`)

This function scans *all* git tags to find the highest non-prerelease version for a given package.

**How it identifies which tags belong to which package:**

Tags follow a tera template (e.g., `{{ package }}-v{{ version }}` for multi-package, `v{{ version }}` for single). Instead of trying to reverse-engineer arbitrary templates, the function renders the template with a known placeholder version `"0.0.0-placeholder"`:

```rust
let rendered = project.git_tag(package_name, "0.0.0-placeholder").ok()?;
let (prefix, suffix) = rendered.split_once("0.0.0-placeholder")?;
```

For a multi-package workspace with package `mylib`, this gives `prefix = "mylib-v"`, `suffix = ""`. For a custom template like `release-{{ package }}-{{ version }}-prod`, it gives `prefix = "release-mylib-"`, `suffix = "-prod"`.

Then it iterates all tags, strips the prefix and suffix, attempts to parse the remainder as a semver version, and keeps only those with `version.pre.is_empty()` — i.e., only stable versions:

```rust
for tag in &all_tags {
    if let Some(version_str) = tag
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
    {
        if let Ok(version) = Version::parse(version_str) {
            if version.pre.is_empty() { // ← the key filter
```

It tracks the highest such version and resolves its commit hash via `repository.get_tag_commit(tag)`. If no stable tag exists at all, it returns `None`.

**Why this is correct for all template formats:** `split_once` on the placeholder decomposes the template into the literal prefix and suffix around the version slot. `strip_prefix`/`strip_suffix` then extracts the exact version string from any tag. Tags for other packages won't match because their prefix differs. Tags with RC versions (like `mylib-v1.2.0-rc.1`) will parse as valid semver but get rejected by the `pre.is_empty()` check.

### 2. `get_diff()` uses the stable baseline for ALL modes (`updater.rs:770-789`)

Previously, `get_diff()` built the tag from `package.version`:

```rust
let git_tag = self.project.git_tag(&package.name, &package.version.to_string())?;
let tag_commit = repository.get_tag_commit(&git_tag);
```

Now it **always** calls `find_last_stable_tag()` first:

```rust
let (git_tag, tag_commit) =
    if let Some(stable_info) =
        find_last_stable_tag(self.project, &package.name, repository)
    {
        let tag = self.project.git_tag(&package.name, &stable_info.version.to_string())?;
        diff.base_version = Some(stable_info.version);
        (tag, Some(stable_info.commit))
    } else {
        // No stable tag found — fall back to constructing tag from package version.
        let tag = self.project.git_tag(&package.name, &package.version.to_string())?;
        let commit = repository.get_tag_commit(&tag);
        (tag, commit)
    };
```

This applies to **all** modes, not just RC/Stable. Even in Default mode, if a package's `Cargo.toml` says `1.2.0-rc.1`, we diff from the `1.2.0` tag, not the `1.2.0-rc.1` tag.

**What `diff.base_version` stores:** When a stable tag is found, `diff.base_version = Some(stable_info.version)` — e.g., `Some(1.2.0)`. This is a stable version with an empty prerelease field. When no stable tag exists (new package, first release), `base_version` stays `None`.

**Why this doesn't break existing behavior for stable packages:** If a package is at `1.2.0` (no RC) and tags `pkg-v1.2.0` and `pkg-v1.1.0` exist, `find_last_stable_tag` returns `1.2.0` — exactly the same version that the old code would have used. The commit walk in `get_package_diff` then proceeds identically because `tag_commit` points to the same commit.

**How `tag_commit` feeds into the commit walk:** The `tag_commit` value is passed to `get_package_diff()` as the stopping point. Inside `get_package_diff`, the function `is_commit_too_old()` checks if the current commit being walked is an ancestor of `tag_commit`:

```rust
if let Some(tag_commit) = tag_commit.as_ref()
    && repository.is_ancestor(current_commit_hash, tag_commit)
{
    return true; // stop walking
}
```

So when we set `tag_commit` to the commit of the stable tag `1.2.0`, the walk goes from HEAD backwards and stops when it reaches the commit tagged `pkg-v1.2.0`. All commits between that point and HEAD — including commits made during `1.2.0-rc.1`, `1.2.0-rc.2`, etc. — are collected. This is exactly what we want: the full diff since the last stable release.

### 3. Bypassing the prerelease short-circuit (`updater.rs:1034-1044`, `version.rs:12-22`)

The `next_from_diff()` trait method at `version.rs:13` is:

```rust
fn next_from_diff(&self, diff: &Diff, version_updater: VersionUpdater) -> Self {
    ...
    version_updater.increment(self, diff.commits.iter().map(|c| &c.message))
}
```

This calls `VersionUpdater::increment(self, version, commits)` which calls `VersionIncrement::from_commits_with_updater(updater, version, commits)`. That function checks `!current_version.pre.is_empty()` at line 70 — if the version passed as `self` has a prerelease, it short-circuits.

The fix is in `get_next_version()` at line 1041-1044:

```rust
match &diff.base_version {
    Some(base) => base.next_from_diff(diff, version_updater),
    None => p.version.next_from_diff(diff, version_updater),
}
```

When `base_version` is `Some(1.2.0)`, we call `next_from_diff` on `1.2.0` — a version with `pre.is_empty() == true`. This means the prerelease check at line 70 does NOT trigger, and conventional commit analysis proceeds normally:
- `fix:` commits → `Patch` → `1.2.1`
- `feat:` commits → `Minor` → `1.3.0`
- `feat!:` commits → `Major` → `2.0.0`

When `base_version` is `None` (no stable tag found, or package already at a stable version), it falls through to the existing `p.version.next_from_diff()` — unchanged behavior.

The same `base_version` substitution is applied in two other places where `next_from_diff` is called:

- **`get_version_groups()`** at line 367-368: `let base = diff.base_version.as_ref().unwrap_or(&pkg.version);` — so version groups also use the stable baseline.
- **`new_workspace_version()`** at line 403-404: same pattern — workspace-versioned packages also get the stable baseline.

### 4. `BumpLevel` enum — Making bump levels comparable (`version.rs:25-58`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BumpLevel {
    None,   // 0
    Patch,  // 1
    Minor,  // 2
    Major,  // 3
}
```

The `#[derive(PartialOrd, Ord)]` on an enum uses variant declaration order. So `None < Patch < Minor < Major`. This enables `max()` to work directly — `BumpLevel::Patch.max(BumpLevel::Major) == BumpLevel::Major`.

**`BumpLevel::compute(base, target)`** at line 37-47: Compares two `Version` values to determine what level of bump occurred. It checks major first, then minor, then patch. This is used to translate the output of `version_updater.increment()` (which returns a concrete `Version`) into a `BumpLevel` for propagation.

**`BumpLevel::apply(base)`** at line 50-57: Applies the bump to a base version using the existing `NextVersion` trait methods (`increment_major/minor/patch`). These methods are defined in `next_version.rs:48-84` and correctly zero out lower components (e.g., `increment_minor` on `1.2.3` → `1.3.0`).

### 5. The two-pass flow in `packages_to_update_rc_stable()` (`updater.rs:183-354`)

This method only runs when `release_mode` is `Rc` or `Stable`. The Default mode runs `packages_to_update_default()` which is the original code extracted into its own method.

**Pass 1 — Per-package bump from own commits (lines 192-246):**

For each package, it:
1. Gets `base_version` from `diff.base_version` (set by `get_diff()`), falling back to `p.version`.
2. If the package has no commits and exists in the registry → `own_bump = BumpLevel::None`. Still tracked for dependency propagation.
3. If the package is new (not in registry) → `own_bump = BumpLevel::None`, uses Cargo.toml version as-is.
4. Otherwise, calls `version_updater.increment(&base_version, commits)` — passing the **stable** `base_version` to avoid the prerelease bypass — then computes `BumpLevel::compute(&base_version, &next_from_commits)`.

Everything is stored in `Pass1Entry { package, diff, own_bump, base_version }`.

**Transitive propagation (lines 248-292):**

```rust
let ordered_names: Vec<String> = self
    .packages_to_process()
    .iter()
    .map(|p| p.name.to_string())
    .collect();
```

`packages_to_process()` returns packages in release order (from `release_order()` in `project.rs:94`). Release order is a topological sort where dependencies come **before** dependents (see `release_order.rs:7-18`). So when we iterate `ordered_names`, leaves (packages with no workspace deps) are processed first.

For each package in order:

```rust
let max_dep_bump = package
    .dependencies
    .iter()
    .filter(|d| matches!(d.kind, Normal | Build))
    .filter_map(|d| final_bumps.get(d.name.as_str()))
    .copied()
    .max()
    .unwrap_or(BumpLevel::None);

let final_bump = entry.own_bump.max(max_dep_bump);
final_bumps.insert(pkg_name.as_str(), final_bump);
```

It looks up the already-computed `final_bump` of each Normal/Build dependency (guaranteed to exist because topological order processes deps first), takes the `max()`, and combines with its own bump via `max()`.

**Why a single pass suffices:** In a topological order, every dependency of package X appears before X. So when we process X, all its deps have their final bumps already computed and stored in `final_bumps`. No iterative fixpoint needed.

**Example:**
- C (leaf): own commits → `feat:` → own_bump = Minor. No deps → final_bump = Minor.
- B depends on C: own commits → `fix:` → own_bump = Patch. max_dep_bump = Minor (from C). final_bump = max(Patch, Minor) = Minor.
- A depends on B: no own commits → own_bump = None. max_dep_bump = Minor (from B). final_bump = Minor.

Result: A gets a Minor bump even though it has no commits — the significance of C's change propagated through B to A.

**Pass 2 — Final version assembly (lines 294-353):**

For each package:
1. Skip if `final_bump == None` and package exists in registry (no change needed).
2. Compute `target_stable = final_bump.apply(&base_version)`. E.g., Minor applied to `1.2.0` → `1.3.0`.
3. For new packages, use `package.version` as-is (the Cargo.toml version).
4. If `ReleaseMode::Rc`: call `find_next_rc_number()` to determine N, then construct `Version { pre: "rc.N", ..target_stable }`. If `ReleaseMode::Stable`: use `target_stable` directly.
5. Generate changelog via `calculate_update_result()` with the `final_version` and the package's own commits from Pass 1.

### 6. `find_next_rc_number()` — RC suffix numbering (`updater.rs:1367-1393`)

Given package `mylib` and target stable `1.3.0`, it builds the string `"1.3.0-rc."`, renders it through the tag template to get e.g. `"mylib-v1.3.0-rc."`, then checks all existing tags:

```rust
for tag in &all_tags {
    if let Some(rc_num_str) = tag.strip_prefix(&rendered) {
        if let Ok(n) = rc_num_str.parse::<u64>() {
            max_rc = max_rc.max(n);
        }
    }
}
max_rc + 1
```

If tags `mylib-v1.3.0-rc.1` and `mylib-v1.3.0-rc.2` exist → max_rc = 2 → returns 3. If none exist → max_rc = 0 → returns 1.

**Why this uses `git_tag()` with a synthetic version string:** The version `"1.3.0-rc."` is not a valid semver, but that doesn't matter — `git_tag()` just renders a tera template, performing string substitution. The result is a string prefix we can match against existing tags. We're not parsing this as semver; we're using it as a literal tag prefix.

### 7. Default mode is fully preserved (`packages_to_update_default`, `updater.rs:80-181`)

The dispatch at line 67-77:

```rust
match self.req.release_mode() {
    ReleaseMode::Rc | ReleaseMode::Stable => {
        self.packages_to_update_rc_stable(packages_diffs, repository)
    }
    ReleaseMode::Default => {
        self.packages_to_update_default(packages_diffs, local_manifest_path)
    }
}
```

`packages_to_update_default()` is the original `packages_to_update()` body extracted verbatim — it still uses the single-pass flow, `get_version_groups()`, `new_workspace_version()`, `dependent_packages_update()`, etc. The only difference in Default mode compared to pre-change behavior is:

1. `get_diff()` now finds the stable tag via `find_last_stable_tag()` instead of blindly using `package.version` for tag construction. For packages already at a stable version, `find_last_stable_tag` returns that exact same version — so behavior is identical. For packages at an RC version, it now correctly uses the stable baseline.

2. `get_next_version()`, `get_version_groups()`, and `new_workspace_version()` now use `diff.base_version` (the stable version) instead of `p.version` when computing `next_from_diff`. Again, for stable packages `base_version` equals `p.version`, so this is a no-op. For RC packages, this avoids the prerelease short-circuit.

## End-to-End Example

Consider a workspace where package A has `Cargo.toml` version `1.2.0-rc.1`. The last published version on crates.io is `1.1.0`. The git tags are `A-v1.1.0`, `A-v1.2.0-rc.1`.

### Before these changes

1. `get_diff()` constructs tag `A-v1.2.0-rc.1` from `package.version`.
2. Walks commits from that RC tag forward — sees only commits made *after* the RC was tagged.
3. `get_next_version()` calls `p.version.next_from_diff()` with `p.version = 1.2.0-rc.1`.
4. Inside `VersionIncrement::from_commits_with_updater()`, the check `!current_version.pre.is_empty()` hits — `1.2.0-rc.1` has a non-empty prerelease.
5. Returns `Prerelease` unconditionally → bumps to `1.2.0-rc.2`.
6. A `feat!:` breaking change commit produces `1.2.0-rc.2` — identical to a typo fix. Commit semantics are ignored.

### After these changes

1. `get_diff()` calls `find_last_stable_tag()` → finds `A-v1.1.0` (skips `A-v1.2.0-rc.1` because `1.2.0-rc.1` has a non-empty prerelease).
2. Sets `diff.base_version = Some(1.1.0)`. Walks commits from the `A-v1.1.0` tag forward — sees ALL commits since the last stable release.
3. `get_next_version()` sees `diff.base_version = Some(1.1.0)`, calls `base.next_from_diff()` with base = `1.1.0`.
4. Inside `VersionIncrement::from_commits_with_updater()`, `current_version` is `1.1.0` — `pre.is_empty()` is true, so the check at line 70 does NOT trigger.
5. Conventional commit analysis runs normally: `feat!:` → Major → `2.0.0`. `feat:` → Minor → `1.2.0`. `fix:` → Patch → `1.1.1`.
6. In RC mode, this then becomes `2.0.0-rc.1` or `1.2.0-rc.1` or `1.1.1-rc.1`. In Stable mode, the version is used directly. In Default mode, the stable version is returned as the next version.

## What Stays Unchanged

- **`version_increment.rs`**: Untouched. The prerelease bypass at line 70-71 still exists — we avoid triggering it by passing a stable `base_version` rather than modifying the logic itself.
- **`next_version.rs`**: Untouched. `increment_major/minor/patch/prerelease` all work as before.
- **`package_dependencies.rs`**: Untouched. `dependencies_to_update()` is reused by the Default mode's `dependent_packages_update()`.
- **`mod.rs` update flow**: `update_manifests()`, `update_changelogs()`, `update_cargo_lock()` — all unchanged. They consume the `PackagesUpdate` output regardless of which mode produced it.
- **CLI, config, `release_pr` code**: No changes. `ReleaseMode` defaults to `Default`, so all existing CLI invocations behave identically.

## Files Modified

| File | Change |
|------|--------|
| `update_request.rs` | Added `ReleaseMode` enum (`Default`, `Rc`, `Stable`), `release_mode` field on `UpdateRequest`, builder method `with_release_mode()`, accessor `release_mode()`. |
| `mod.rs` (update) | Re-exported `ReleaseMode` so it is accessible as `release_plz_core::ReleaseMode`. |
| `diff.rs` | Added `base_version: Option<Version>` field to `Diff`, initialized to `None` in `Diff::new()`. |
| `version.rs` | Added `BumpLevel` enum with `compute()` and `apply()` helpers. Added 8 unit tests. |
| `updater.rs` | Added `find_last_stable_tag()`, `find_next_rc_number()`. Changed `get_diff()` to always use stable baseline. Changed `get_next_version()`, `get_version_groups()`, `new_workspace_version()` to use `base_version`. Split `packages_to_update()` into `packages_to_update_default()` and `packages_to_update_rc_stable()` with two-pass propagation. |
