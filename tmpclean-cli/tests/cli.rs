#![cfg(windows)]
//! Real-surface tests: the built `tmpclean.exe`, the real resolver (TMP/TEMP
//! point at a fixture named `Temp`), and the real Windows filesystem.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::Command;
use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// `<unique dir>\Temp`, seeded with nested files; removed on drop.
struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let parent = std::env::var_os("RUNNER_TEMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = parent.join(format!(
            "tmpclean-cli-{label}-{}-{unique}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("Temp");
        fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("sub").join("b.txt"), b"bravo").unwrap();
        fs::write(root.join("sub").join("deep").join("c.txt"), b"charlie").unwrap();
        Self { base, root }
    }

    fn intact(&self) -> bool {
        self.root.join("a.txt").is_file()
            && self.root.join("sub").join("b.txt").is_file()
            && self.root.join("sub").join("deep").join("c.txt").is_file()
    }

    fn entries(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    /// The exe with TMP/TEMP pointing at the fixture and the shared test flags.
    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("tmpclean").unwrap();
        command
            .env("TMP", &self.root)
            .env("TEMP", &self.root)
            .env_remove("TMPCLEAN_IO_PERMITS")
            .args(["--targets", "user-temp", "--min-age", "0"]);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.base) {
            eprintln!(
                "fixture cleanup failed for {}: {error}",
                self.base.display()
            );
        }
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be exactly one JSON document ({error}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn counter(report: &Value, name: &str) -> u64 {
    report["counters"][name]
        .as_u64()
        .unwrap_or_else(|| panic!("missing counter {name}: {report}"))
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("process exit code")
}

fn icacls(path: &Path, args: &[&str]) -> io::Result<()> {
    let output = StdCommand::new("icacls").arg(path).args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "icacls {} {args:?}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stdout)
        )))
    }
}

/// Removes the Everyone deny ACEs before the fixture is deleted.
struct DenyGuard(Vec<PathBuf>);

impl Drop for DenyGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            if let Err(error) = icacls(path, &["/remove:d", "*S-1-1-0"]) {
                eprintln!("ACL restore failed: {error}");
            }
        }
    }
}

#[test]
fn dry_run_json_deletes_nothing_and_emits_schema_1() {
    let fixture = Fixture::new("dry-run");
    let output = fixture
        .command()
        .args(["--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(fixture.intact());
    let report = json(&output);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["targets"][0]["name"], "user-temp");
    assert_eq!(report["targets"][0]["status"], "resolved");
    assert!(report["targets"][0]["path"]
        .as_str()
        .unwrap()
        .ends_with("\\Temp"));
    assert_eq!(counter(&report, "files_removed"), 3, "would-remove total");
}

#[test]
fn unknown_target_is_usage_error() {
    let fixture = Fixture::new("bogus");
    let output = Command::cargo_bin("tmpclean")
        .unwrap()
        .env("TMP", &fixture.root)
        .env("TEMP", &fixture.root)
        .args(["--targets", "bogus", "--yes"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 2, "{output:?}");
    assert!(fixture.intact());
}

#[test]
fn piped_stdin_without_yes_refuses_and_keeps_fixture() {
    let fixture = Fixture::new("noconfirm");
    // Even a piped "y" must not count as confirmation.
    let output = fixture.command().write_stdin("y\n").output().unwrap();
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("confirmation required; pass --yes"));
    assert!(output.stdout.is_empty());
    assert!(fixture.intact());
}

fn target<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("missing targets: {report}"))
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("missing target {name}: {report}"))
}

#[test]
fn piped_stdin_json_without_yes_emits_refusal_report() {
    let fixture = Fixture::new("noconfirm-json");
    let output = fixture
        .command()
        .arg("--json")
        .write_stdin("y\n")
        .output()
        .unwrap();
    assert_eq!(code(&output), 3, "{output:?}");
    let report = json(&output);
    assert_eq!(report["schema"], 1, "{report}");
    assert_eq!(report["exit_code"], 3, "{report}");
    assert_eq!(report["outcome"], "refused", "{report}");
    assert_eq!(report["dry_run"], false);
    assert_eq!(target(&report, "user-temp")["status"], "resolved");
    let counters = report["counters"].as_object().unwrap();
    assert_eq!(counters.len(), 10, "{report}");
    assert!(counters.values().all(|c| c == 0), "{report}");
    assert_eq!(report["errors"], serde_json::json!([]));
    assert!(String::from_utf8_lossy(&output.stderr).contains("confirmation required; pass --yes"));
    assert!(fixture.intact());
}

/// Deterministic mixed run: only `user-temp` is redirectable (TMP/TEMP), so
/// it is made the refused target by pointing it at a real directory whose
/// name is not on the allow-list (`name-not-allowed`); `windows-temp`
/// resolves on the elevated CI runner. `--dry-run` keeps the real
/// Windows\Temp untouched while the scheduler still processes it. Before the
/// fix the resolved root made this exit 0.
#[test]
fn mixed_refused_and_resolved_targets_exit_3() {
    let fixture = Fixture::new("mixed");
    let refused = fixture.base.join("NotTemp");
    fs::create_dir_all(&refused).unwrap();
    let keep = refused.join("keep.txt");
    fs::write(&keep, b"keep").unwrap();
    let output = Command::cargo_bin("tmpclean")
        .unwrap()
        .env("TMP", &refused)
        .env("TEMP", &refused)
        .env_remove("TMPCLEAN_IO_PERMITS")
        .args([
            "--targets",
            "user-temp,windows-temp",
            "--min-age",
            "0",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&output), 3, "{output:?}");
    let report = json(&output);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["exit_code"], 3, "{report}");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["targets"].as_array().unwrap().len(), 2, "{report}");
    assert_eq!(
        target(&report, "user-temp")["status"],
        "refused",
        "{report}"
    );
    let windows_temp = target(&report, "windows-temp");
    assert_eq!(windows_temp["status"], "resolved", "{report}");
    assert!(windows_temp["path"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .ends_with("\\temp"));
    // The runnable target was processed (the scheduler ran to an outcome).
    assert!(
        report["outcome"]
            .as_str()
            .is_some_and(|o| o.starts_with("completed")),
        "{report}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("user-temp: refused"));
    assert!(keep.is_file(), "the refused target is never touched");
    assert!(fixture.intact());
}

#[test]
fn yes_empties_fixture_and_keeps_root() {
    let fixture = Fixture::new("yes");
    let output = fixture
        .command()
        .args(["--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    let report = json(&output);
    assert_eq!(report["outcome"], "completed");
    assert_eq!(counter(&report, "files_removed"), 3);
    assert_eq!(counter(&report, "dirs_removed"), 2);
    assert!(fixture.root.is_dir(), "the temp root itself is kept");
    assert_eq!(fixture.entries(), Vec::<PathBuf>::new());
}

#[test]
fn text_mode_prints_logical_bytes_summary() {
    let fixture = Fixture::new("text");
    let output = fixture.command().arg("--yes").output().unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Logical bytes removed: 17"), "{stdout}");
    assert!(!stdout.trim_start().starts_with('{'));
}

#[test]
fn locked_file_is_skipped_and_ancestors_kept() {
    let fixture = Fixture::new("locked");
    let parent = fixture.root.join("l1").join("l2");
    fs::create_dir_all(&parent).unwrap();
    let locked = parent.join("locked.bin");
    fs::write(&locked, b"locked").unwrap();
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked)
        .unwrap();
    let output = fixture
        .command()
        .args(["--yes", "--json"])
        .output()
        .unwrap();
    drop(handle);
    assert_eq!(code(&output), 0, "{output:?}");
    let report = json(&output);
    assert_eq!(counter(&report, "skipped_in_use"), 1, "{report}");
    assert_eq!(counter(&report, "failed"), 0, "{report}");
    assert!(locked.is_file());
    assert!(fixture.root.join("l1").is_dir() && parent.is_dir());
    assert!(!fixture.root.join("a.txt").exists());
    assert!(!fixture.root.join("sub").exists());
}

#[test]
fn delete_denied_file_fails_with_exit_1() {
    let fixture = Fixture::new("denied");
    let denied = fixture.root.join("denied.txt");
    fs::write(&denied, b"denied").unwrap();
    // FILE_DELETE_CHILD on the parent would otherwise override the file's
    // DELETE deny, so deny both; the guard drops before the fixture.
    let _guard = DenyGuard(vec![denied.clone(), fixture.root.clone()]);
    icacls(&denied, &["/deny", "*S-1-1-0:(D)"]).unwrap();
    icacls(&fixture.root, &["/deny", "*S-1-1-0:(DC)"]).unwrap();
    let output = fixture
        .command()
        .args(["--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 1, "{output:?}");
    let report = json(&output);
    assert_eq!(report["exit_code"], 1);
    assert_eq!(counter(&report, "failed"), 1, "{report}");
    assert_eq!(report["errors"][0]["disposition"], "failed");
    assert!(denied.is_file());
}

#[test]
fn io_permits_flag_accepted() {
    let fixture = Fixture::new("permits");
    let output = fixture
        .command()
        .args(["--yes", "--json", "--io-permits", "1"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(json(&output)["exit_code"], 0);
    assert_eq!(fixture.entries(), Vec::<PathBuf>::new());
}

#[test]
fn malformed_io_permits_env_and_min_age_are_usage_errors() {
    let fixture = Fixture::new("bad-input");
    let output = fixture
        .command()
        .env("TMPCLEAN_IO_PERMITS", "abc")
        .args(["--yes"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 2, "{output:?}");
    // The flag wins over the environment.
    let output = fixture
        .command()
        .env("TMPCLEAN_IO_PERMITS", "abc")
        .args(["--dry-run", "--json", "--io-permits", "2"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    for min_age in ["5", "-1h", "1w"] {
        let output = Command::cargo_bin("tmpclean")
            .unwrap()
            .env("TMP", &fixture.root)
            .env("TEMP", &fixture.root)
            .args(["--targets", "user-temp", "--yes", "--min-age", min_age])
            .output()
            .unwrap();
        assert_eq!(code(&output), 2, "{min_age}: {output:?}");
    }
    assert!(fixture.intact());
}
