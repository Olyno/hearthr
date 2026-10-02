use std::path::Path;

use crate::spec::{Command, ServiceFile};

/// The production executor: run the command, capture its output, and turn
/// a non-zero exit into the OS's own words. No shell is involved: argv is
/// handed to the binary directly, which is the injection floor this whole
/// crate keeps.
pub fn exec(cmd: &Command) -> Result<String, String> {
    let out = std::process::Command::new(&cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", cmd.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    Err(format!(
        "exited {}: {}",
        out.status,
        if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        }
    ))
}

// --- The files on disk -------------------------------------------------------

/// Write one install file. `secret` files get owner-only mode on unix;
/// the other platforms take the plain write, whose access control is the
/// directory the file sits in.
pub(crate) fn write_file(file: &ServiceFile) -> Result<(), String> {
    if file.secret {
        return write_secret_file(&file.path, &file.contents);
    }
    std::fs::create_dir_all(file.path.parent().unwrap_or(Path::new(".")))
        .map_err(|e| format!("creating {}: {e}", file.path.display()))?;
    std::fs::write(&file.path, &file.contents)
        .map_err(|e| format!("writing {}: {e}", file.path.display()))
}

/// Write a file that must only be readable by its owner. Unix creates it
/// with mode 0600 and re-asserts the mode on an existing file (a reinstall
/// must not inherit a looser one).
#[cfg(unix)]
pub(crate) fn write_secret_file(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    // An existing file keeps its old mode through `.mode()`; set it again
    // so the 0600 law is about the file, not about how it was born.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// The non-unix write: secrecy is the data directory's own ACL (per-user
/// by construction on Windows).
#[cfg(not(unix))]
pub(crate) fn write_secret_file(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Remove a file, shrugging at "it was never there": an uninstall of a
/// half-install must still finish.
pub(crate) fn remove_quiet(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", path.display())),
    }
}

/// The env-file spelling of a launch environment: one `KEY=value` per
/// line, values double-quoted with C-style escapes (the systemd
/// `EnvironmentFile` grammar, which every adapter's own syntax is derived
/// from); `#` comments first, so a human opening the file sees what it is
/// and who owns it.
pub(crate) fn env_to_text(env: &[(String, String)]) -> String {
    let mut out = String::from(
        "# hearthr service — launch environment.\n\
         # Written by the installing application; owner-only (0600).\n\
         # Uninstalling the service removes this file with it.\n",
    );
    for (key, value) in env {
        out.push_str(key);
        out.push('=');
        out.push_str(&env_quote(value));
        out.push('\n');
    }
    out
}

/// Quote a value for the env-file grammar: wrap in double quotes and
/// escape backslashes and quotes (systemd re-reads `\\` and `\"`).
pub(crate) fn env_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_grammar_quotes_the_carriers() {
        let text = env_to_text(&[
            ("A".into(), "plain".into()),
            ("B".into(), "with\"and\\ ".into()),
        ]);
        assert!(text.contains("A=\"plain\"\n"));
        assert!(text.contains("B=\"with\\\"and\\\\ \"\n"));
    }

    #[test]
    fn a_broken_command_reports_the_os_words_not_a_panic() {
        let err = exec(&Command::new("this-tool-does-not-exist-hearthr", vec![])).unwrap_err();
        assert!(err.contains("this-tool-does-not-exist-hearthr"), "{err}");
    }
}
