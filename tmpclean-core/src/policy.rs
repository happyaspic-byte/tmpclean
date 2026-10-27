//! Shared safety decisions, independent of CLI prompts or GUI countdowns.
//!
//! Reparse points, including volume mount points, are leaves. Only their link is
//! disposed; a failed link deletion is reported rather than traversing its target.
//! Handle-relative traversal does not promise protection against hostile
//! concurrent reparse mutation under elevation. Do not run elevated against a
//! tree controlled by a hostile process on the assumption that links are immutable.

use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConfirmState {
    Confirmed,
    Declined,
    #[default]
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    /// Counter fields are would-remove totals in preview mode, never mutations.
    pub dry_run: bool,
    /// Zero disables age filtering. Directories are never age-filtered.
    pub min_age: Duration,
    /// Surfaces parse --io-permits / TMPCLEAN_IO_PERMITS as a positive integer.
    /// The per-device override remains subject to the global worker ceiling.
    pub io_permits: Option<NonZeroUsize>,
    pub confirm: ConfirmState,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            dry_run: false,
            min_age: Duration::from_secs(24 * 60 * 60),
            io_permits: None,
            confirm: ConfirmState::Required,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    ConfirmationRequired,
    ConfirmationDeclined,
}

impl Refusal {
    pub const fn exit_code(self) -> u8 {
        3
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ConfirmationRequired => "confirmation-required",
            Self::ConfirmationDeclined => "confirmation-declined",
        })
    }
}

impl std::error::Error for Refusal {}

impl Policy {
    pub fn check(self) -> Result<(), Refusal> {
        if self.dry_run || self.confirm == ConfirmState::Confirmed {
            return Ok(());
        }
        Err(match self.confirm {
            ConfirmState::Declined => Refusal::ConfirmationDeclined,
            _ => Refusal::ConfirmationRequired,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::fs::{
        BasicInfo, FailureMode, FakeFs, FileSystem, FsError, Operation, ReparseTag, READONLY,
    };
    use crate::report::{Clock, Counters, Outcome};
    use crate::sched::Scheduler;

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Duration {
            Duration::from_secs(1_800_000_000)
        }
    }

    #[test]
    fn policy_required_and_declined_refuse_before_touching_filesystem() {
        for (confirm, refusal) in [
            (ConfirmState::Required, Refusal::ConfirmationRequired),
            (ConfirmState::Declined, Refusal::ConfirmationDeclined),
        ] {
            let fs = FakeFs::new();
            let before = fs.stats();
            let policy = Policy {
                confirm,
                ..Policy::default()
            };
            assert!(
                matches!(Engine::new(&fs, &FixedClock, policy), Err(error) if error == refusal)
            );
            assert!(
                matches!(Scheduler::new(&fs, FixedClock, policy), Err(error) if error == refusal)
            );
            assert_eq!(refusal.exit_code(), 3);
            assert_eq!(fs.stats(), before);
            println!("{confirm:?}: engine and scheduler refused, exit 3, fake untouched");
        }
    }

    #[test]
    fn policy_dryrun_thirty_entries_has_exact_would_remove_totals() {
        let fs = FakeFs::new();
        fs.close(fs.add_dir("dir").unwrap());
        for i in 0..28 {
            fs.close(
                fs.add_file(
                    format!("dir/file{i}"),
                    BasicInfo {
                        attributes: READONLY,
                        size: 2,
                        ..BasicInfo::default()
                    },
                )
                .unwrap(),
            );
        }
        fs.close(
            fs.add_reparse("mount", ReparseTag::MountPoint, "/outside")
                .unwrap(),
        );
        for confirm in [ConfirmState::Required, ConfirmState::Declined] {
            let policy = Policy {
                dry_run: true,
                min_age: Duration::ZERO,
                confirm,
                ..Policy::default()
            };
            let engine = Engine::new(&fs, &FixedClock, policy).unwrap();
            let result = engine.run(fs.root_handle(), "/");
            assert_eq!(
                result.counters,
                Counters {
                    files_removed: 28,
                    dirs_removed: 1,
                    links_removed: 1,
                    logical_bytes_removed: 56,
                    ..Counters::default()
                }
            );
            assert_eq!(result.outcome, Outcome::Completed { fully_empty: false });
            assert_eq!(fs.stats().disposition_calls, 0);
            assert_eq!(fs.stats().clear_readonly_calls, 0);
            assert_eq!(engine.pending_references(), 0);
            println!(
                "dry-run {confirm:?}: 30 would-remove; {:?}; zero dispositions/readonly calls",
                result.counters
            );
        }
    }

    #[test]
    fn policy_mountpoint_is_a_leaf_even_when_link_deletion_fails() {
        for fail in [false, true] {
            let fs = FakeFs::new();
            let root = fs.add_dir("temp").unwrap();
            fs.close(fs.add_dir("outside").unwrap());
            fs.close(fs.add_file("outside/secret", BasicInfo::default()).unwrap());
            fs.close(
                fs.add_reparse("temp/mount", ReparseTag::MountPoint, "/outside")
                    .unwrap(),
            );
            fs.close(fs.add_file("temp/sibling", BasicInfo::default()).unwrap());
            if fail {
                fs.inject_failure(
                    "temp/mount",
                    Operation::SetDisposition,
                    FsError::Other(999),
                    FailureMode::Once,
                )
                .unwrap();
            }
            let policy = Policy {
                confirm: ConfirmState::Confirmed,
                min_age: Duration::ZERO,
                ..Policy::default()
            };
            let result = Engine::new(&fs, &FixedClock, policy)
                .unwrap()
                .run(root, "/temp");
            assert_eq!(result.counters.links_removed, u64::from(!fail));
            assert_eq!(result.counters.failed, u64::from(fail));
            assert_eq!(result.counters.files_removed, 1);
            assert!(fs
                .stats()
                .enumerated_directories
                .iter()
                .all(|path| path == std::path::Path::new("/temp")));
            if fail {
                assert_eq!(result.errors[0].code, 999);
                assert_eq!(result.errors[0].disposition, "failed");
            }
            println!(
                "mount failure={fail}: {:?}; target never enumerated",
                result.counters
            );
        }
    }
}
