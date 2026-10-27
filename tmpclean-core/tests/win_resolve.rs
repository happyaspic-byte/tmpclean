#![cfg(windows)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tmpclean_core::fs::{FileSystem, DIRECTORY};
use tmpclean_core::resolve::{allowed_name, normalize_path, Resolution, Target};
use tmpclean_core::win::resolve::{resolve_target, resolve_targets};
use tmpclean_core::win::WinFs;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tmpclean-resolve-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let junction = self.0.join("link").join("Temp");
        if junction.symlink_metadata().is_ok() {
            fs::remove_dir(junction).unwrap();
        }
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn check_resolved(result: Resolution<tmpclean_core::win::WinHandle>) -> String {
    match result {
        Resolution::Resolved { handle, final_path } => {
            assert!(allowed_name(final_path.rsplit('\\').next().unwrap()));
            assert!(!final_path.starts_with(r"\\?\"));
            assert_ne!(
                WinFs::new().basic_info(&handle).unwrap().attributes & DIRECTORY,
                0
            );
            // Bypass libtest's successful-test capture so CI records the real paths.
            writeln!(io::stdout().lock(), "resolve real path: {final_path}").unwrap();
            final_path
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

#[test]
fn resolve_real_user_temp() {
    check_resolved(resolve_target(Target::UserTemp));
}

#[test]
fn resolve_real_inet_cache() {
    match resolve_target(Target::InetCache) {
        resolved @ Resolution::Resolved { .. } => {
            check_resolved(resolved);
        }
        Resolution::Absent => writeln!(io::stdout().lock(), "inet-cache: Absent").unwrap(),
        other => panic!("inet-cache must be Resolved or Absent: {other:?}"),
    }
}

fn child(path: &Path, expected: &str) {
    // Only the child sees the override. Other tests never observe mutated env.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resolve_child_env", "--nocapture"])
        .env("TMP", path)
        .env("TEMP", path)
        .env("TMPCLEAN_RESOLVE_EXPECTED", expected)
        .env("TMPCLEAN_RESOLVE_PATH", path)
        .output()
        .unwrap();
    io::stdout().lock().write_all(&output.stdout).unwrap();
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("resolve child assertion executed"),
        "child test filter did not execute the assertion"
    );
}

#[test]
fn resolve_child_env() {
    let Ok(expected) = std::env::var("TMPCLEAN_RESOLVE_EXPECTED") else {
        return; // Invoked by the test harness; the parent supplies the child cases.
    };
    let result = resolve_target(Target::UserTemp);
    println!("resolve child {expected}: {result:?}");
    match expected.as_str() {
        "resolved" => {
            let path = check_resolved(result);
            assert!(path.eq_ignore_ascii_case(&normalize_path(
                &std::env::var("TMPCLEAN_RESOLVE_PATH").unwrap()
            )));
        }
        "absent" => assert!(matches!(result, Resolution::Absent)),
        reason => assert!(matches!(result, Resolution::Refused(actual) if actual == reason)),
    }
    // A refusal in one target must not short-circuit the following target.
    let results = resolve_targets(&[Target::UserTemp, Target::InetCache]);
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].0, Target::InetCache);
    assert!(matches!(
        results[1].1,
        Resolution::Resolved { .. } | Resolution::Absent
    ));
    println!("resolve child assertion executed");
}

#[test]
fn resolve_env_temp_fixture() {
    let fixture = Fixture::new();
    let path = fixture.0.join("Temp");
    fs::create_dir(&path).unwrap();
    child(&path, "resolved");
}

#[test]
fn resolve_env_name_refused() {
    let fixture = Fixture::new();
    let path = fixture.0.join("NotTemp");
    fs::create_dir(&path).unwrap();
    child(&path, "name-not-allowed");
}

#[test]
fn resolve_env_absent() {
    let fixture = Fixture::new();
    child(&fixture.0.join("Temp"), "absent");
}

#[test]
fn resolve_env_junction_refused() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("destination");
    fs::create_dir(&destination).unwrap();
    fs::create_dir(fixture.0.join("link")).unwrap();
    let junction = fixture.0.join("link").join("Temp");
    let output = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    child(&junction, "reparse-root");
}
