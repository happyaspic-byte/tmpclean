//! Command-line flags. Pure parsing, unit-tested on every host.

use std::num::NonZeroUsize;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tmpclean_core::resolve::Target;
use tmpclean_core::strings;

#[derive(Debug, Parser)]
#[command(name = "tmpclean", version = tmpclean_core::VERSION, about = strings::CLI_ABOUT)]
pub struct Cli {
    #[arg(
        long,
        help = strings::CLI_HELP_TARGETS,
        value_enum,
        value_delimiter = ',',
        value_name = "LIST",
        default_values_t = [TargetArg::UserTemp, TargetArg::InetCache]
    )]
    pub targets: Vec<TargetArg>,

    #[arg(long, help = strings::CLI_HELP_DRY_RUN)]
    pub dry_run: bool,

    #[arg(long, help = strings::CLI_HELP_YES)]
    pub yes: bool,

    #[arg(
        long,
        help = strings::CLI_HELP_MIN_AGE,
        value_name = "DUR",
        default_value = "24h",
        value_parser = parse_duration
    )]
    pub min_age: Duration,

    #[arg(long, help = strings::CLI_HELP_JSON)]
    pub json: bool,

    #[arg(
        long,
        help = strings::CLI_HELP_IO_PERMITS,
        value_name = "N",
        env = "TMPCLEAN_IO_PERMITS"
    )]
    pub io_permits: Option<NonZeroUsize>,

    #[arg(long, hide = true, help = strings::CLI_HELP_STATS)]
    pub stats: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TargetArg {
    UserTemp,
    InetCache,
    WindowsTemp,
}

impl From<TargetArg> for Target {
    fn from(arg: TargetArg) -> Self {
        match arg {
            TargetArg::UserTemp => Target::UserTemp,
            TargetArg::InetCache => Target::InetCache,
            TargetArg::WindowsTemp => Target::WindowsTemp,
        }
    }
}

impl Cli {
    /// Requested targets in first-mention order, duplicates removed.
    pub fn targets(&self) -> Vec<Target> {
        let mut out: Vec<Target> = Vec::new();
        for &arg in &self.targets {
            let target = Target::from(arg);
            if !out.contains(&target) {
                out.push(target);
            }
        }
        out
    }
}

/// `0` or a non-negative integer with one of the suffixes `s`, `m`, `h`, `d`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let expected = || format!("`{text}`: {}", strings::DURATION_EXPECTED);
    let unit = text.chars().last().ok_or(strings::DURATION_EMPTY)?;
    let seconds_per_unit: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return Err(expected()),
    };
    let digits = &text[..text.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(expected());
    }
    let count: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}`: {}", strings::DURATION_NUMBER_TOO_LARGE))?;
    count
        .checked_mul(seconds_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}`: {}", strings::DURATION_TOO_LARGE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("tmpclean").chain(args.iter().copied()))
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_duration("24h"), Ok(Duration::from_secs(86_400)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(604_800)));
        for bad in [
            "",
            "5",
            "h",
            "-1h",
            "+1h",
            "1.5h",
            "1w",
            "abc",
            "99999999999999999999d",
            "１h",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn defaults() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.targets(), [Target::UserTemp, Target::InetCache]);
        assert_eq!(cli.min_age, Duration::from_secs(86_400));
        assert!(!cli.dry_run && !cli.yes && !cli.json);
    }

    #[test]
    fn targets_parse_and_dedupe() {
        let cli = parse(&["--targets", "windows-temp,user-temp,windows-temp"]).unwrap();
        assert_eq!(cli.targets(), [Target::WindowsTemp, Target::UserTemp]);
    }

    #[test]
    fn usage_errors() {
        for args in [
            &["--targets", "bogus"][..],
            &["--targets", ""],
            &["--min-age", "5"],
            &["--io-permits", "0"],
            &["--io-permits", "abc"],
            &["--frobnicate"],
        ] {
            let error = parse(args).unwrap_err();
            assert_ne!(error.kind(), ErrorKind::DisplayHelp, "{args:?}");
            assert_eq!(error.exit_code(), 2, "{args:?}");
        }
        assert_eq!(
            parse(&["--io-permits", "1"]).unwrap().io_permits,
            NonZeroUsize::new(1)
        );
    }
}
