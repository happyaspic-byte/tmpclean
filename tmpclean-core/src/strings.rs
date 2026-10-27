//! All user-facing English strings for tmpclean.
//! Keeping them in one table makes a future localization (e.g. Korean)
//! a one-file change.

pub const APP_NAME: &str = "tmpclean";

// GUI window (tmpclean-gui). The done title is an observable test signal.
pub const GUI_TITLE: &str = "tmpclean";
pub const GUI_TITLE_DONE: &str = "tmpclean - done";
pub const GUI_BUTTON_CANCEL: &str = "Cancel";
pub const GUI_BUTTON_CLOSE: &str = "Close";
pub const GUI_TARGETS: &str = "Targets";
pub const GUI_COUNTDOWN_PREFIX: &str = "Starting in ";
pub const GUI_COUNTDOWN_SUFFIX: &str = " s - Enter: start now, Esc: cancel";
pub const GUI_RUNNING: &str = "Cleaning...";
pub const GUI_RUNNING_DRY: &str = "Dry run: counting what would be removed...";
pub const GUI_CANCELING: &str = "Canceling...";
pub const GUI_DONE_PREFIX: &str = "Done";
pub const GUI_EXIT_CODE: &str = "exit code";
pub const GUI_DRY_RUN_NOTE: &str = "Dry run: counters are would-remove totals.";
pub const GUI_WINDOW_FAILED: &str = "Unable to create the tmpclean window.";
pub const GUI_WINDOWS_ONLY: &str = "tmpclean-gui runs only on Windows.";

// Report / summary labels.
pub const LABEL_OUTCOME: &str = "Outcome";
pub const LABEL_FILES_REMOVED: &str = "Files removed";
pub const LABEL_DIRS_REMOVED: &str = "Directories removed";
pub const LABEL_LINKS_REMOVED: &str = "Links removed";
pub const LABEL_CLASSIC_PENDING: &str = "Classic items pending confirmation";
pub const LABEL_EXCLUDED: &str = "Excluded by policy (min-age)";
pub const LABEL_SKIPPED_IN_USE: &str = "Skipped (in use)";
pub const LABEL_FAILED: &str = "Failed";
pub const LABEL_ABSENT: &str = "Already absent";
pub const LABEL_CANCELED: &str = "Canceled";
pub const LABEL_LOGICAL_BYTES_REMOVED: &str = "Logical bytes removed";

// Outcome names.
pub const OUTCOME_COMPLETED: &str = "completed";
pub const OUTCOME_COMPLETED_NOT_EMPTY: &str = "completed (not fully empty)";
pub const OUTCOME_CANCELED: &str = "canceled";
pub const OUTCOME_FAILED_TO_ENUMERATE: &str = "failed to enumerate";
/// JSON outcome of a CLI run refused before it started (machine token).
pub const OUTCOME_REFUSED: &str = "refused";

// Resolve status names (also used in --json targets[].status).
pub const STATUS_RESOLVED: &str = "resolved";
pub const STATUS_ABSENT: &str = "absent";
pub const STATUS_REQUIRES_ELEVATION: &str = "requires-elevation";
pub const STATUS_REFUSED: &str = "refused";
pub const STATUS_UNKNOWN_TARGET: &str = "unknown-target";

// Confirmation / cancel messaging.
pub const CONFIRM_PROMPT: &str = "Proceed with deletion? [y/N]";
pub const CONFIRM_DECLINED: &str = "Confirmation declined; nothing was deleted.";
pub const CANCELED_BY_USER: &str = "Canceled by user.";

// CLI surface.
pub const CONFIRM_TARGETS: &str = "The contents of these folders will be deleted:";
pub const CONFIRMATION_REQUIRED: &str = "confirmation required; pass --yes";
pub const DRY_RUN_NOTE: &str =
    "Dry run: nothing was deleted; removal counts are would-remove totals.";
pub const LABEL_PROGRESS: &str = "Progress";
pub const MORE_ERRORS_IN_JSON: &str = "More errors occurred; run with --json for the full list.";
pub const WINDOWS_ONLY: &str = "Windows only";
pub const CTRL_C_UNAVAILABLE: &str = "warning: Ctrl+C handler unavailable";

// CLI flag help (tmpclean --help).
pub const CLI_ABOUT: &str = "Fast, safe Windows temp folder cleaner.";
pub const CLI_HELP_TARGETS: &str = "Comma-separated targets to clean.";
pub const CLI_HELP_DRY_RUN: &str = "Count what would be deleted; delete nothing.";
pub const CLI_HELP_YES: &str =
    "Confirm deletion without prompting (required when stdin is not a terminal).";
pub const CLI_HELP_MIN_AGE: &str =
    "Skip files and links changed more recently than DUR (e.g. 0, 90s, 30m, 24h, 7d).";
pub const CLI_HELP_JSON: &str = "Print only the JSON report (schema v1) to stdout.";
pub const CLI_HELP_IO_PERMITS: &str =
    "Concurrent deletions per physical device (default: chosen per device).";
pub const CLI_HELP_STATS: &str = "Print scheduler diagnostics to stderr after the run.";

// --min-age validation (shown after the offending value).
pub const DURATION_EMPTY: &str = "empty duration";
pub const DURATION_EXPECTED: &str = "expected 0 or a number with s, m, h or d";
pub const DURATION_NUMBER_TOO_LARGE: &str = "number too large";
pub const DURATION_TOO_LARGE: &str = "duration too large";
