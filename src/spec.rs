use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The names the three OS runners know this service by. Each platform
/// spells identifiers in its own grammar (a unit file stem, a reverse-DNS
/// label, a free-text task name), so the spec carries all three rather
/// than deriving them from a stem no platform agrees on. None of them
/// derives from a moving app identifier: a service registered under one
/// name must not answer under another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitId {
    /// systemd user unit file name, e.g. `mydoor.service`.
    pub systemd: String,
    /// launchd job label, e.g. `com.example.mydoor`.
    pub launchd: String,
    /// Task Scheduler task name, e.g. `MyApp Door`.
    pub windows: String,
}

/// What the service must run and with what arguments. Every path is
/// resolved by the caller at install time; the artifact templates
/// interpolate nothing but these.
///
/// The credential is deliberately absent: the launch environment arrives
/// as a pair list at install/sync time, and the secret's own storage (an
/// OS keyring, a vault, anywhere) is the caller's policy. `hearthr` only
/// carries it down to the files the OS manager reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    /// The per-platform names.
    pub id: UnitId,
    /// Human-readable one-liner the unit carries as its description.
    pub display_name: String,
    /// The binary the service runs — a path the OS will still resolve
    /// after the installing app is gone (a stable, published location;
    /// never a build directory that a rebuild empties).
    pub binary: PathBuf,
    /// The service's command line, minus any credential.
    pub args: Vec<String>,
    /// Extra argv appended only on the unix units: flags whose meaning
    /// relies on the launch environment being a 0600 file the unit
    /// re-reads (e.g. `--key-watch <env_file>`, `--no-keystore`).
    #[serde(default)]
    pub unix_args: Vec<String>,
    /// Extra argv appended only on the Windows wrapper (e.g. a tray flag
    /// every platform's door carries but the unix ones receive through
    /// [`ServiceSpec::unix_args`]).
    #[serde(default)]
    pub windows_args: Vec<String>,
    /// Environment names that must not reach the Windows task
    /// environment: values whose characters cmd's quoting cannot promise
    /// to carry verbatim, and any secret the wrapper's on-disk form
    /// should not hold. The pair list arrives complete; this is the
    /// Windows filter, applied by the wrapper renderer.
    #[serde(default)]
    pub windows_env_drop: Vec<String>,
    /// The 0600 file holding the endpoint launch environment: systemd's
    /// `EnvironmentFile=`, and the pair list the other adapters render
    /// inline from. It exists even where the OS format carries the pairs
    /// itself — it is the caller's re-render source of truth.
    pub env_file: PathBuf,
    /// The caller-owned data dir: holds the spec file the install leaves
    /// behind, the Windows artifacts, and the service's stderr log.
    pub data_dir: PathBuf,
}

impl ServiceSpec {
    /// The exec words for a unix unit: binary, args, unix extras.
    pub(crate) fn unix_exec(&self) -> Vec<String> {
        let mut words = vec![self.binary.to_string_lossy().into_owned()];
        words.extend(self.args.iter().cloned());
        words.extend(self.unix_args.iter().cloned());
        words
    }

    /// The exec words for the Windows wrapper: binary, args, windows
    /// extras.
    pub(crate) fn windows_exec(&self) -> Vec<String> {
        let mut words = vec![self.binary.to_string_lossy().into_owned()];
        words.extend(self.args.iter().cloned());
        words.extend(self.windows_args.iter().cloned());
        words
    }

    /// The Windows-filtered launch environment: every pair except the
    /// names this spec declares cmd-unsafe. The rule is exact keys, never
    /// a prefix — a prefix would one day drop the bearer list itself.
    pub(crate) fn windows_env(&self, env: &[(String, String)]) -> Vec<(String, String)> {
        env.iter()
            .filter(|(key, _)| !self.windows_env_drop.iter().any(|drop| drop == key))
            .cloned()
            .collect()
    }

    /// The log file the Windows/launchd artifacts point stderr into: a
    /// daemon nobody reads from must still tell someone why it died.
    pub(crate) fn log_path(&self) -> PathBuf {
        self.data_dir.join(format!(
            "{}.log",
            self.id.systemd.trim_end_matches(".service")
        ))
    }
}

/// What a lifecycle call answers with. The UI branches on these fields
/// and never parses a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceState {
    /// Which OS runner owns the service: `systemd-user`, `launchd`,
    /// `schtasks`, or `none` when this machine has no adapter.
    pub kind: String,
    /// The unit/task identifier the OS knows it by (empty when `none`).
    pub unit: String,
    /// The artifact is registered.
    pub installed: bool,
    /// The OS reports the unit running right now (probed only when
    /// installed).
    pub running: bool,
    /// The OS's own words on a refusal or the last command's output.
    /// Shown as detail, never parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One OS command an adapter wants run: the program and its argv, never
/// a shell string — nothing in this crate goes through a command
/// interpreter, which is the injection floor the whole design keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// A command whose failure the sequence tolerates: `bootout` of a job
    /// that is not loaded and `/End` of a task that never ran are how an
    /// idempotent install starts from a clean slate, and the OS says no
    /// in its own words to a step nothing depends on.
    pub lenient: bool,
}

impl Command {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Command {
        Command {
            program: program.into(),
            args,
            lenient: false,
        }
    }

    pub fn lenient(program: impl Into<String>, args: Vec<String>) -> Command {
        Command {
            program: program.into(),
            args,
            lenient: true,
        }
    }

    /// The command as one line, for logs and error messages only —
    /// never executed as a string.
    pub fn display(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
    }
}

/// One file an adapter's install writes. `secret` files are created 0600
/// (owner-only); a unit file is never one, so its content can be
/// inspected by anything that can read `~/.config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFile {
    pub path: PathBuf,
    pub contents: String,
    pub secret: bool,
}
