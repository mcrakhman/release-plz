use cargo_metadata::semver::Version;
use next_version::{NextVersion as _, VersionIncrement, VersionUpdater};

use crate::{diff::Diff, semver_check::SemverCheck};

pub(crate) trait NextVersionFromDiff {
    /// Analyze commits and determine which part of version to increment based on
    /// [conventional commits](https://www.conventionalcommits.org/)
    fn next_from_diff(&self, diff: &Diff, version_updater: VersionUpdater) -> Self;
}

impl NextVersionFromDiff for Version {
    fn next_from_diff(&self, diff: &Diff, version_updater: VersionUpdater) -> Self {
        if !diff.should_update_version() {
            self.clone()
        } else if matches!(diff.semver_check, SemverCheck::Incompatible(_)) {
            let increment = VersionIncrement::breaking(self);
            increment.bump(self)
        } else {
            version_updater.increment(self, diff.commits.iter().map(|c| &c.message))
        }
    }
}

/// Represents the level of a version bump.
/// Ordered so that `None < Patch < Minor < Major`, enabling `max()` propagation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BumpLevel {
    None,
    Patch,
    Minor,
    Major,
}

impl BumpLevel {
    /// Compare `base` and `target` to determine what level of bump occurred.
    pub fn compute(base: &Version, target: &Version) -> Self {
        if target.major > base.major {
            Self::Major
        } else if target.minor > base.minor {
            Self::Minor
        } else if target.patch > base.patch {
            Self::Patch
        } else {
            Self::None
        }
    }

    /// Apply this bump level to a base version, producing a new stable version.
    pub fn apply(&self, base: &Version) -> Version {
        match self {
            Self::Major => base.increment_major(),
            Self::Minor => base.increment_minor(),
            Self::Patch => base.increment_patch(),
            Self::None => base.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::diff::Commit;

    use crate::NO_COMMIT_ID;

    use super::*;

    #[test]
    fn next_version_of_new_package_is_unchanged() {
        let registry_package_exists = false;
        let diff = Diff::new(registry_package_exists);
        let version = Version::new(1, 2, 3);
        assert_eq!(
            version
                .clone()
                .next_from_diff(&diff, VersionUpdater::default()),
            version
        );
    }

    #[test]
    fn next_version_of_existing_package_is_updated() {
        let diff = Diff {
            registry_package_exists: true,
            commits: vec![Commit::new(
                NO_COMMIT_ID.to_string(),
                "my change".to_string(),
            )],
            is_version_published: true,
            semver_check: SemverCheck::Skipped,
            registry_version: None,
            base_version: None,
        };
        let version = Version::new(1, 2, 3);
        assert_eq!(
            version.next_from_diff(&diff, VersionUpdater::default()),
            Version::new(1, 2, 4)
        );
    }

    #[test]
    fn next_version_doesnt_bump_0_x_minor_version_for_features() {
        let diff = Diff {
            registry_package_exists: true,
            commits: vec![Commit::new(
                NO_COMMIT_ID.to_string(),
                "feat: my change".to_string(),
            )],
            is_version_published: true,
            semver_check: SemverCheck::Skipped,
            registry_version: None,
            base_version: None,
        };
        let version = Version::new(0, 2, 3);
        assert_eq!(
            version.next_from_diff(&diff, VersionUpdater::default()),
            Version::new(0, 2, 4)
        );
    }

    #[test]
    fn next_version_bumps_0_x_minor_version_for_features() {
        let diff = Diff {
            registry_package_exists: true,
            commits: vec![Commit::new(
                NO_COMMIT_ID.to_string(),
                "feat: my change".to_string(),
            )],
            is_version_published: true,
            semver_check: SemverCheck::Skipped,
            registry_version: None,
            base_version: None,
        };
        let version = Version::new(0, 2, 3);
        let updater = VersionUpdater::default().with_features_always_increment_minor(true);
        assert_eq!(
            version.next_from_diff(&diff, updater),
            Version::new(0, 3, 0)
        );
    }

    // ── BumpLevel tests ──

    #[test]
    fn bump_level_compute_major() {
        let base = Version::new(1, 0, 0);
        let target = Version::new(2, 0, 0);
        assert_eq!(BumpLevel::compute(&base, &target), BumpLevel::Major);
    }

    #[test]
    fn bump_level_compute_minor() {
        let base = Version::new(1, 0, 0);
        let target = Version::new(1, 1, 0);
        assert_eq!(BumpLevel::compute(&base, &target), BumpLevel::Minor);
    }

    #[test]
    fn bump_level_compute_patch() {
        let base = Version::new(1, 0, 0);
        let target = Version::new(1, 0, 1);
        assert_eq!(BumpLevel::compute(&base, &target), BumpLevel::Patch);
    }

    #[test]
    fn bump_level_compute_none() {
        let base = Version::new(1, 0, 0);
        assert_eq!(BumpLevel::compute(&base, &base), BumpLevel::None);
    }

    #[test]
    fn bump_level_apply_major() {
        let base = Version::new(1, 2, 3);
        assert_eq!(BumpLevel::Major.apply(&base), Version::new(2, 0, 0));
    }

    #[test]
    fn bump_level_apply_minor() {
        let base = Version::new(1, 2, 3);
        assert_eq!(BumpLevel::Minor.apply(&base), Version::new(1, 3, 0));
    }

    #[test]
    fn bump_level_apply_patch() {
        let base = Version::new(1, 2, 3);
        assert_eq!(BumpLevel::Patch.apply(&base), Version::new(1, 2, 4));
    }

    #[test]
    fn bump_level_apply_none() {
        let base = Version::new(1, 2, 3);
        assert_eq!(BumpLevel::None.apply(&base), Version::new(1, 2, 3));
    }

    #[test]
    fn bump_level_ordering() {
        assert!(BumpLevel::None < BumpLevel::Patch);
        assert!(BumpLevel::Patch < BumpLevel::Minor);
        assert!(BumpLevel::Minor < BumpLevel::Major);
    }

    #[test]
    fn bump_level_max_propagation() {
        // Simulates: package A has patch bump, depends on B with major bump.
        // A should get major.
        let a_own = BumpLevel::Patch;
        let b_bump = BumpLevel::Major;
        let a_final = a_own.max(b_bump);
        assert_eq!(a_final, BumpLevel::Major);
    }
}
