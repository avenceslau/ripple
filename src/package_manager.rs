use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::git::ChangedFile;
use crate::workspace::Package;

/// Impact of a change to a package manager's root lockfile.
pub enum LockfileImpact {
    /// Workspace package names whose resolved external dependency closure changed.
    Modeled(BTreeSet<String>),
    /// The change could not be attributed precisely; every target is conservatively
    /// affected. The string explains why.
    Fallback(String),
}

/// A JavaScript package manager whose root lockfile monoripple can attribute to
/// workspace packages. Each implementation is a specialization keyed by the
/// lockfile file name(s) it owns.
pub trait PackageManager {
    /// Root lockfile file names this manager owns.
    fn lockfile_names(&self) -> &'static [&'static str];

    /// Detail recorded on a package seeded by a modeled lockfile change.
    fn modeled_detail(&self) -> &'static str {
        "changed dependency resolution"
    }

    /// Attribute a change to one of this manager's lockfiles to workspace packages.
    fn lockfile_impact(
        &self,
        root: &Path,
        base: &str,
        change: &ChangedFile,
        packages: &[Package],
    ) -> Result<LockfileImpact>;
}

const MANAGERS: &[&dyn PackageManager] = &[&crate::pnpm::Pnpm, &Npm, &Yarn, &Bun];

/// The package manager that owns `file_name`, if any.
pub fn package_manager_for(file_name: &str) -> Option<&'static dyn PackageManager> {
    MANAGERS
        .iter()
        .copied()
        .find(|manager| manager.lockfile_names().contains(&file_name))
}

const UNMODELED_FORMAT: &str = "exact runtime consumers are not modeled for this lockfile format";

struct Npm;

impl PackageManager for Npm {
    fn lockfile_names(&self) -> &'static [&'static str] {
        &["package-lock.json"]
    }

    fn lockfile_impact(
        &self,
        _root: &Path,
        _base: &str,
        _change: &ChangedFile,
        _packages: &[Package],
    ) -> Result<LockfileImpact> {
        Ok(LockfileImpact::Fallback(UNMODELED_FORMAT.to_string()))
    }
}

struct Yarn;

impl PackageManager for Yarn {
    fn lockfile_names(&self) -> &'static [&'static str] {
        &["yarn.lock"]
    }

    fn lockfile_impact(
        &self,
        _root: &Path,
        _base: &str,
        _change: &ChangedFile,
        _packages: &[Package],
    ) -> Result<LockfileImpact> {
        Ok(LockfileImpact::Fallback(UNMODELED_FORMAT.to_string()))
    }
}

struct Bun;

impl PackageManager for Bun {
    fn lockfile_names(&self) -> &'static [&'static str] {
        &["bun.lock", "bun.lockb"]
    }

    fn lockfile_impact(
        &self,
        _root: &Path,
        _base: &str,
        _change: &ChangedFile,
        _packages: &[Package],
    ) -> Result<LockfileImpact> {
        Ok(LockfileImpact::Fallback(UNMODELED_FORMAT.to_string()))
    }
}
