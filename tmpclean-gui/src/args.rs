//! Command line: the same flag set as the `tmpclean` CLI.

use std::num::NonZeroUsize;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tmpclean_core::resolve::Target;
use tmpclean_core::strings;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TargetArg {
    UserTemp,
    InetCache,
    WindowsTemp,
}

impl From<TargetArg> for Target {
    fn from(t: TargetArg) -> Self {
        match t {
            TargetArg::UserTemp => Target::UserTemp,
            TargetArg::InetCache => Target::InetCache,
            TargetArg::WindowsTemp => Target::WindowsTemp,
        }
    }
}

/// Fast, safe Windows temp folder cleaner (window).
#[derive(Debug, Parser)]
#[command(name = "tmpclean-gui", version = tmpclean_core::VERSION)]
pub struct Cli {
    /// Comma-separated targets to clean.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "user-temp,inet-cache"
    )]
    pub targets: Vec<TargetArg>,
    /// Count what would be removed without deleting anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Start immediately, without the 5 s countdown.
    #[arg(long)]
    pub yes: bool,
    /// Skip files modified more recently than this (e.g. 90s, 30m, 24h, 7d; 0 disables).
    #[arg(long, default_value = "24h", value_parser = parse_min_age)]
    pub min_age: Duration,
    /// Accepted for CLI parity; the window shows the summary instead.
    #[arg(long)]
    pub json: bool,
    /// Per-device I/O permits override.
    #[arg(long, env = "TMPCLEAN_IO_PERMITS")]
    pub io_permits: Option<NonZeroUsize>,
}

impl Cli {
    /// Requested targets, de-duplicated in order.
    pub fn target_list(&self) -> Vec<Target> {
        let mut out: Vec<Target> = Vec::new();
        for t in self.targets.iter().map(|&t| Target::from(t)) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }
}

/// `0`, or a positive integer with an optional `s`/`m`/`h`/`d` unit (default seconds).
pub fn parse_min_age(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (digits, scale) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    let n: u64 = digits.parse().map_err(|_| {
        format!(
            "{}{s:?}{}",
            strings::GUI_ARG_DURATION_INVALID_PREFIX,
            strings::GUI_ARG_DURATION_INVALID_SUFFIX
        )
    })?;
    n.checked_mul(scale)
        .map(Duration::from_secs)
        .ok_or_else(|| {
            format!(
                "{}{s:?}{}",
                strings::GUI_ARG_DURATION_TOO_LARGE_PREFIX,
                strings::GUI_ARG_DURATION_TOO_LARGE_SUFFIX
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_args_defaults_match_cli_contract() {
        let cli = Cli::try_parse_from(["tmpclean-gui"]).unwrap();
        assert_eq!(cli.target_list(), [Target::UserTemp, Target::InetCache]);
        assert_eq!(cli.min_age, Duration::from_secs(24 * 3600));
        assert!(!cli.yes && !cli.dry_run && !cli.json);
    }

    #[test]
    fn gui_args_full_flag_set() {
        let cli = Cli::try_parse_from([
            "tmpclean-gui",
            "--targets",
            "user-temp,windows-temp,user-temp",
            "--dry-run",
            "--yes",
            "--min-age",
            "0",
            "--json",
            "--io-permits",
            "3",
        ])
        .unwrap();
        assert_eq!(cli.target_list(), [Target::UserTemp, Target::WindowsTemp]);
        assert!(cli.dry_run && cli.yes && cli.json);
        assert_eq!(cli.min_age, Duration::ZERO);
        assert_eq!(cli.io_permits, NonZeroUsize::new(3));
    }

    #[test]
    fn gui_args_malformed_input_is_usage_error_2() {
        for bad in [
            &["tmpclean-gui", "--targets", "bogus"][..],
            &["tmpclean-gui", "--min-age", "soon"],
            &["tmpclean-gui", "--io-permits", "0"],
            &["tmpclean-gui", "--frobnicate"],
        ] {
            let err = Cli::try_parse_from(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn gui_args_min_age_units() {
        for (s, secs) in [
            ("0", 0),
            ("90", 90),
            ("90s", 90),
            ("30m", 1800),
            ("24h", 86_400),
            ("7d", 604_800),
        ] {
            assert_eq!(parse_min_age(s), Ok(Duration::from_secs(secs)), "{s}");
        }
        for s in ["", "h", "-1h", "1.5h", "1w"] {
            assert!(parse_min_age(s).is_err(), "{s}");
        }
        assert_eq!(
            parse_min_age("soon"),
            Err("invalid duration \"soon\": expected e.g. 0, 90s, 30m, 24h, 7d".into())
        );
        assert_eq!(
            parse_min_age("99999999999999999d"),
            Err("duration \"99999999999999999d\" is too large".into())
        );
    }
}
