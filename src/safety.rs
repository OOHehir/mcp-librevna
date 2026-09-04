//! The policy layer: what an agent is allowed to ask the instrument to do.
//!
//! Everything here runs *before* any SCPI is transmitted, so a refused request
//! leaves instrument state untouched. Three separate concerns live here:
//!
//! - **Capability tiers** gate whole classes of operation behind startup flags.
//! - **The stimulus ceiling** caps output power below what the device permits.
//! - **The path sandbox** confines calibration and Touchstone files.
//!
//! Parameter range checking lives with [`crate::device::DeviceLimits`] instead,
//! because those bounds are facts about the hardware rather than policy.

use std::path::{Component, Path, PathBuf};

use crate::device::DeviceLimits;
use crate::error::{SafetyError, Tier};

/// A conservative default output level.
///
/// Well below the LibreVNA's own maximum, chosen so that attaching an unknown
/// or sensitive DUT is unlikely to be eventful. Raise it deliberately.
pub const DEFAULT_MAX_STIMULUS_DBM: f64 = -10.0;

/// What this server instance is permitted to do.
#[derive(Debug, Clone)]
pub struct Policy {
    allow_emission: bool,
    allow_destructive: bool,
    allow_manual_hardware: bool,
    max_stimulus_dbm: f64,
    /// Canonicalised at construction, so later comparisons are symlink-free.
    workdir: PathBuf,
}

impl Policy {
    /// Build a policy rooted at `workdir`, which must already exist.
    pub fn new(workdir: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            allow_emission: false,
            allow_destructive: false,
            allow_manual_hardware: false,
            max_stimulus_dbm: DEFAULT_MAX_STIMULUS_DBM,
            workdir: workdir.as_ref().canonicalize()?,
        })
    }

    pub fn with_emission(mut self, allow: bool) -> Self {
        self.allow_emission = allow;
        self
    }

    pub fn with_destructive(mut self, allow: bool) -> Self {
        self.allow_destructive = allow;
        self
    }

    pub fn with_manual_hardware(mut self, allow: bool) -> Self {
        self.allow_manual_hardware = allow;
        self
    }

    pub fn with_max_stimulus_dbm(mut self, dbm: f64) -> Self {
        self.max_stimulus_dbm = dbm;
        self
    }

    pub fn max_stimulus_dbm(&self) -> f64 {
        self.max_stimulus_dbm
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Whether a tier is enabled, without producing an error.
    pub fn allows(&self, tier: Tier) -> bool {
        match tier {
            Tier::ReadOnly | Tier::Measure => true,
            Tier::Emission => self.allow_emission,
            Tier::Destructive => self.allow_destructive,
            Tier::ManualHardware => self.allow_manual_hardware,
        }
    }

    /// Require a tier, naming the operation and the flag that would enable it.
    pub fn require(&self, tier: Tier, operation: &str) -> Result<(), SafetyError> {
        if self.allows(tier) {
            Ok(())
        } else {
            Err(SafetyError::TierNotEnabled {
                operation: operation.to_string(),
                tier,
                flag: tier.flag(),
            })
        }
    }

    /// Check a requested stimulus level against both the ceiling and the device.
    ///
    /// The device's own range is checked first: an out-of-range value is a
    /// mistake worth reporting as such, even when the ceiling would also
    /// reject it.
    pub fn check_stimulus(&self, dbm: f64, limits: &DeviceLimits) -> Result<(), SafetyError> {
        limits.check_power(dbm)?;
        if dbm > self.max_stimulus_dbm {
            return Err(SafetyError::PowerCeiling {
                requested: dbm,
                ceiling: self.max_stimulus_dbm,
            });
        }
        Ok(())
    }

    /// Resolve a caller-supplied path inside the working directory.
    ///
    /// Rejects traversal (`../`), absolute paths pointing elsewhere, and
    /// symlinks whose target escapes. Because the file being written may not
    /// exist yet, the check is lexical first, then re-checked against the
    /// deepest ancestor that does exist -- which is what catches a symlinked
    /// directory pointing out of the sandbox.
    pub fn resolve_path(&self, requested: &str) -> Result<PathBuf, SafetyError> {
        let escape = || SafetyError::PathEscape {
            path: requested.to_string(),
            workdir: self.workdir.display().to_string(),
        };

        if requested.trim().is_empty() || requested.contains('\0') {
            return Err(escape());
        }

        let raw = Path::new(requested);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.workdir.join(raw)
        };

        let normalised = lexically_normalise(&joined);
        if !normalised.starts_with(&self.workdir) {
            return Err(escape());
        }

        // Walk back to the nearest existing ancestor and resolve it for real,
        // so a symlinked subdirectory cannot smuggle the path outside.
        let mut ancestor = normalised.as_path();
        loop {
            match ancestor.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(&self.workdir) {
                        return Err(escape());
                    }
                    break;
                }
                Err(_) => match ancestor.parent() {
                    Some(parent) => ancestor = parent,
                    // Ran out of ancestors without finding anything real; the
                    // lexical check above already proved containment.
                    None => break,
                },
            }
        }

        Ok(normalised)
    }
}

/// Resolve `.` and `..` textually, without consulting the filesystem.
fn lexically_normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Tier;

    fn limits() -> DeviceLimits {
        DeviceLimits {
            min_freq_hz: 100_000.0,
            max_freq_hz: 6.0e9,
            min_ifbw_hz: 10.0,
            max_ifbw_hz: 50_000.0,
            max_points: 4501,
            min_power_dbm: -40.0,
            max_power_dbm: 0.0,
            min_rbw_hz: 10.0,
            max_rbw_hz: 5.0e6,
        }
    }

    /// A policy rooted at a fresh temporary directory.
    fn sandboxed() -> (Policy, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "mcp-librevna-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Policy::new(&dir).unwrap();
        let canonical = dir.canonicalize().unwrap();
        (policy, canonical)
    }

    #[test]
    fn read_and_measure_are_available_without_any_flags() {
        let (policy, _) = sandboxed();
        assert!(policy.require(Tier::ReadOnly, "status").is_ok());
        assert!(policy.require(Tier::Measure, "sweep").is_ok());
    }

    #[test]
    fn the_higher_tiers_are_closed_by_default() {
        let (policy, _) = sandboxed();
        for tier in [Tier::Emission, Tier::Destructive, Tier::ManualHardware] {
            assert!(
                policy.require(tier, "op").is_err(),
                "{tier} should be closed by default"
            );
        }
    }

    #[test]
    fn each_flag_opens_only_its_own_tier() {
        let (base, _) = sandboxed();

        let emission = base.clone().with_emission(true);
        assert!(emission.allows(Tier::Emission));
        assert!(!emission.allows(Tier::Destructive));
        assert!(!emission.allows(Tier::ManualHardware));

        let destructive = base.clone().with_destructive(true);
        assert!(destructive.allows(Tier::Destructive));
        assert!(!destructive.allows(Tier::Emission));
    }

    #[test]
    fn a_refusal_names_the_flag_that_would_allow_it() {
        let (policy, _) = sandboxed();
        let msg = policy
            .require(Tier::Emission, "gen_configure")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("gen_configure"), "{msg}");
        assert!(msg.contains("--allow-emission"), "{msg}");
    }

    #[test]
    fn stimulus_is_capped_by_the_ceiling_below_the_device_maximum() {
        let (policy, _) = sandboxed();
        // The device would allow 0 dBm; the default ceiling is lower.
        assert!(limits().check_power(0.0).is_ok());
        assert!(matches!(
            policy.check_stimulus(0.0, &limits()),
            Err(SafetyError::PowerCeiling { .. })
        ));
        assert!(policy.check_stimulus(-10.0, &limits()).is_ok());
    }

    #[test]
    fn raising_the_ceiling_permits_more_power_but_never_beyond_the_device() {
        let (policy, _) = sandboxed();
        let policy = policy.with_max_stimulus_dbm(10.0);
        // Ceiling raised above the device maximum: the device still governs.
        assert!(policy.check_stimulus(0.0, &limits()).is_ok());
        assert!(matches!(
            policy.check_stimulus(5.0, &limits()),
            Err(SafetyError::OutOfRange { .. })
        ));
    }

    #[test]
    fn plain_filenames_resolve_inside_the_workdir() {
        let (policy, root) = sandboxed();
        let resolved = policy.resolve_path("sweep.s2p").unwrap();
        assert_eq!(resolved, root.join("sweep.s2p"));
    }

    #[test]
    fn nested_paths_are_allowed() {
        let (policy, root) = sandboxed();
        let resolved = policy.resolve_path("cal/today/solt.cal").unwrap();
        assert_eq!(resolved, root.join("cal/today/solt.cal"));
    }

    #[test]
    fn traversal_out_of_the_workdir_is_refused() {
        let (policy, _) = sandboxed();
        for attempt in [
            "../escape.s2p",
            "cal/../../escape.s2p",
            "./../../etc/passwd",
        ] {
            assert!(
                policy.resolve_path(attempt).is_err(),
                "{attempt:?} should be refused"
            );
        }
    }

    #[test]
    fn traversal_that_returns_inside_is_allowed() {
        // `a/../b.s2p` never actually leaves, so refusing it would be wrong.
        let (policy, root) = sandboxed();
        let resolved = policy.resolve_path("a/../b.s2p").unwrap();
        assert_eq!(resolved, root.join("b.s2p"));
    }

    #[test]
    fn absolute_paths_outside_the_workdir_are_refused() {
        let (policy, _) = sandboxed();
        assert!(policy.resolve_path("/etc/passwd").is_err());
        assert!(policy.resolve_path("/tmp").is_err());
    }

    #[test]
    fn absolute_paths_inside_the_workdir_are_allowed() {
        let (policy, root) = sandboxed();
        let inside = root.join("fine.s2p");
        let resolved = policy.resolve_path(inside.to_str().unwrap()).unwrap();
        assert_eq!(resolved, inside);
    }

    #[test]
    fn empty_and_nul_bearing_paths_are_refused() {
        let (policy, _) = sandboxed();
        assert!(policy.resolve_path("").is_err());
        assert!(policy.resolve_path("   ").is_err());
        assert!(policy.resolve_path("bad\0name.s2p").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_the_workdir_is_refused() {
        // The lexical check alone cannot catch this: the path looks contained.
        let (policy, root) = sandboxed();
        let link = root.join("escape-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        let result = policy.resolve_path("escape-link/passwd");
        let _ = std::fs::remove_file(&link);

        assert!(
            result.is_err(),
            "a symlink out of the sandbox must be refused, got {result:?}"
        );
    }
}
