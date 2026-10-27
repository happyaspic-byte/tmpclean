//! Pure root guards. Windows discovery and owned handles live in `win::resolve`.

use crate::report::ResolveStatus;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    UserTemp,
    InetCache,
    /// Only resolve this target when the caller explicitly opts in.
    WindowsTemp,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserTemp => "user-temp",
            Self::InetCache => "inet-cache",
            Self::WindowsTemp => "windows-temp",
        }
    }
}

/// Each target is independent: a refusal does not discard other resolved roots.
#[derive(Debug)]
pub enum Resolution<H> {
    Resolved { handle: H, final_path: String },
    Absent,
    RequiresElevation,
    Refused(&'static str),
}

impl<H> Resolution<H> {
    pub fn status(&self) -> ResolveStatus {
        match self {
            Self::Resolved { .. } => ResolveStatus::Resolved,
            Self::Absent => ResolveStatus::Absent,
            Self::RequiresElevation => ResolveStatus::RequiresElevation,
            Self::Refused(_) => ResolveStatus::Refused,
        }
    }
}

/// Normalize Win32 final paths without turning an extended UNC path into a local one.
pub fn normalize_path(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = if path
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\UNC\"))
    {
        format!(r"\\{}", &path[8..])
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    };
    path.trim_end_matches('\\').to_owned()
}

pub fn allowed_name(name: &str) -> bool {
    [
        "Temp",
        "tmp",
        "SystemTemp",
        "INetCache",
        "Temporary Internet Files",
    ]
    .iter()
    .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// Validate facts obtained from the opened root, not enumeration or the input path.
/// `protected` contains the actual profile, Windows, LocalAppData and Program Files paths.
/// Reasons are stable machine strings and must not be localized.
pub fn validate_root(
    final_path: &str,
    fixed_volume: bool,
    reparse: bool,
    protected: &[String],
) -> Result<(), &'static str> {
    let path = normalize_path(final_path);
    if let Some(unc) = path.strip_prefix(r"\\") {
        let parts = unc.split('\\').filter(|s| !s.is_empty()).count();
        return Err(if parts <= 2 {
            "share-root"
        } else {
            "not-fixed-volume"
        });
    }
    let bytes = path.as_bytes();
    if (bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || (path.to_ascii_lowercase().starts_with("volume{") && !path.contains('\\'))
    {
        return Err("volume-root");
    }
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
        || path.contains('\0')
        || path.split('\\').any(|part| part == "." || part == "..")
    {
        return Err("invalid-path");
    }
    if protected
        .iter()
        .any(|folder| path.eq_ignore_ascii_case(&normalize_path(folder)))
    {
        return Err("protected-folder");
    }
    if reparse {
        return Err("reparse-root");
    }
    if !fixed_volume {
        return Err("not-fixed-volume");
    }
    if !allowed_name(path.rsplit('\\').next().unwrap_or_default()) {
        return Err("name-not-allowed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected() -> Vec<String> {
        [
            r"C:\Users\x",
            r"C:\Windows",
            r"C:\Users\x\AppData\Local",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[test]
    fn resolve_rootguard() {
        for (path, reason) in [
            (r"C:\", "volume-root"),
            (r"\\?\C:\", "volume-root"),
            (r"\\?\Volume{123}\", "volume-root"),
            (r"C:\Users\x", "protected-folder"),
            (r"C:\Windows", "protected-folder"),
            (r"C:\Users\x\AppData\Local", "protected-folder"),
            (r"C:\Program Files", "protected-folder"),
            (r"C:\Program Files (x86)", "protected-folder"),
            (r"\\server\share", "share-root"),
            (r"\\?\UNC\server\share\", "share-root"),
            (r"\\server\share\Temp", "not-fixed-volume"),
            (r"C:\Temp2", "name-not-allowed"),
            (r"C:\Users\other", "name-not-allowed"),
            ("", "invalid-path"),
            (r"C:\Temp\..", "invalid-path"),
        ] {
            let result = validate_root(path, true, false, &protected());
            println!("resolve_rootguard {path:?}: {result:?}");
            assert_eq!(result, Err(reason), "{path}");
        }
    }

    #[test]
    fn resolve_allowed_roots() {
        for path in [
            r"C:\Users\x\AppData\Local\Temp",
            r"C:\Windows\SystemTemp",
            r"C:\Users\x\AppData\Local\Microsoft\Windows\INetCache",
            r"C:\Temp\",
            r"\\?\C:\tEmP\\",
            r"C:\TMP",
            r"C:\Temporary Internet Files",
        ] {
            assert_eq!(validate_root(path, true, false, &protected()), Ok(()));
        }
        assert_eq!(
            validate_root(r"c:\WINDOWS\\", true, false, &protected()),
            Err("protected-folder")
        );
    }

    #[test]
    fn resolve_metadata_guards() {
        assert_eq!(
            validate_root(r"C:\Temp", true, true, &[]),
            Err("reparse-root")
        );
        assert_eq!(
            validate_root(r"C:\Temp", false, false, &[]),
            Err("not-fixed-volume")
        );
    }

    #[test]
    fn resolve_status_mapping() {
        for (resolution, status) in [
            (
                Resolution::Resolved {
                    handle: (),
                    final_path: r"C:\Temp".into(),
                },
                ResolveStatus::Resolved,
            ),
            (Resolution::Absent, ResolveStatus::Absent),
            (
                Resolution::RequiresElevation,
                ResolveStatus::RequiresElevation,
            ),
            (Resolution::Refused("reparse-root"), ResolveStatus::Refused),
        ] {
            assert_eq!(resolution.status(), status);
        }
    }
}
