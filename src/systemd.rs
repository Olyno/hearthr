//! Linux: a systemd **user** unit under `~/.config/systemd/user/`, driven
//! by `systemctl --user`. The unit names the binary and its arguments;
//! the launch environment never appears in it — it rides the `0600`
//! `EnvironmentFile=` beside the data dir, so the unit can sit
//! world-readable in a config dir without leaking a value the caller
//! considers private.
//!
//! Install also asks logind to *linger* this user
//! (`loginctl enable-linger [$USER]`), because `WantedBy=default.target`
//! only answers at login: with `Linger=no` the user manager does not
//! exist until a session starts, so an enabled unit booted to a machine
//! nobody had logged into would never run. Uninstall deliberately does
//! **not** disable it again — lingering is a per-user setting other
//! services (a rootless podman, a backup timer, this user's second
//! install) may have come to rely on, and taking it away on removal would
//! break them for a reason this uninstall never owned.
//! launchd and schtasks need no analog: a LaunchAgent and a LogonTrigger
//! are already login-scoped.
//!
//! The unit path is `home/.config/systemd/user/<name>.service`, matching
//! the compiled-in default of the systemd user manager; a caller who sets
//! `$XDG_CONFIG_HOME` to something else passes that directory's parent as
//! the runner's home.

use std::path::PathBuf;

use crate::runner::OsServiceRunner;
use crate::spec::{Command, ServiceFile, ServiceSpec};

pub struct SystemdUserRunner {
    /// The home whose `.config/systemd/user` holds the unit. Tests inject
    /// a fixed one so generation is deterministic: the adapter never
    /// reads a process environment the caller did not supply.
    home: PathBuf,
    /// The account to ask logind to linger (see the module doc). A field
    /// like the home, never a live probe inside generation: `at()` leaves
    /// it unset — then `loginctl enable-linger` runs bare, which is
    /// systemd's documented spelling for "the caller's session user" —
    /// and the production default resolves `$USER` (then `$LOGNAME`) and
    /// names it explicitly when found.
    linger_user: Option<String>,
}

impl SystemdUserRunner {
    /// The runner writing units under `home`, lingering the caller's own
    /// session account (loginctl's no-argument spelling).
    pub fn at(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            linger_user: None,
        }
    }

    /// Name the account to linger: the injection point tests use to pin
    /// the exact argv.
    pub fn with_linger_user(mut self, user: impl Into<String>) -> Self {
        self.linger_user = Some(user.into());
        self
    }

    /// The unit path for this spec.
    pub fn unit_path(&self, spec: &ServiceSpec) -> PathBuf {
        self.home
            .join(".config")
            .join("systemd")
            .join("user")
            .join(&spec.id.systemd)
    }
}

impl Default for SystemdUserRunner {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        // Name the account to linger when the environment knows it
        // ($USER, then $LOGNAME); an unnamed install falls back to
        // loginctl's own session-user default.
        let user = std::env::var("USER")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| {
                std::env::var("LOGNAME")
                    .ok()
                    .filter(|u| !u.trim().is_empty())
            });
        match user {
            Some(user) => Self::at(home).with_linger_user(user),
            None => Self::at(home),
        }
    }
}

impl OsServiceRunner for SystemdUserRunner {
    fn name(&self) -> &'static str {
        "systemd-user"
    }

    /// The unit file being on disk is installed-ness for systemd user
    /// units: a daemon-reload makes it live even before `enable`.
    fn installed(&self, spec: &ServiceSpec) -> bool {
        self.unit_path(spec).is_file()
    }

    fn install_files(&self, spec: &ServiceSpec, _env: &[(String, String)]) -> Vec<ServiceFile> {
        // The unit file names no secret — the launch environment lives in
        // the 0600 EnvironmentFile the install core wrote beside the
        // spec (systemd is the one OS whose unit reads that file at boot
        // instead of carrying the pairs inline).
        vec![ServiceFile {
            path: self.unit_path(spec),
            contents: unit_file(spec),
            secret: false,
        }]
    }

    fn install_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        let mut linger = vec!["enable-linger".to_string()];
        // argv direct, never a shell string — the injection floor the
        // whole crate keeps. A named account when the runner knows one;
        // bare otherwise, which loginctl answers as the caller's own.
        if let Some(user) = &self.linger_user {
            linger.push(user.clone());
        }
        vec![
            // First: make the user manager exist at boot, or `enable`
            // registers a unit nobody will start until the next login
            // (see the module doc). Lenient: a machine where logind
            // refuses (container, unusual PAM) still installs the unit —
            // it will answer the next login, just not a headless boot.
            Command::lenient("loginctl", linger),
            Command::new("systemctl", vec!["--user".into(), "daemon-reload".into()]),
            Command::new(
                "systemctl",
                vec![
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    spec.id.systemd.clone(),
                ],
            ),
        ]
    }

    fn uninstall_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![
            Command::new(
                "systemctl",
                vec![
                    "--user".into(),
                    "disable".into(),
                    "--now".into(),
                    spec.id.systemd.clone(),
                ],
            ),
            Command::new("systemctl", vec!["--user".into(), "daemon-reload".into()]),
        ]
    }

    fn uninstall_files(&self, spec: &ServiceSpec) -> Vec<PathBuf> {
        vec![self.unit_path(spec)]
    }

    fn start_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![Command::new(
            "systemctl",
            vec!["--user".into(), "start".into(), spec.id.systemd.clone()],
        )]
    }

    fn stop_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![Command::new(
            "systemctl",
            vec!["--user".into(), "stop".into(), spec.id.systemd.clone()],
        )]
    }

    fn restart_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        // A bare `restart`, not a reload+restart pair: the environment
        // bounces re-write the 0600 EnvironmentFile, which systemd
        // re-reads at every unit start, and the unit's own text is a pure
        // function of the spec — an unchanged file has nothing to reload.
        // Where the unit text DOES change the writer is the install path,
        // and `install_cmds` already pays the daemon-reload there. The
        // bounce is on the caller's apply clock; a reload is half a
        // second it must not spend.
        vec![Command::new(
            "systemctl",
            vec!["--user".into(), "restart".into(), spec.id.systemd.clone()],
        )]
    }

    fn running(&self, spec: &ServiceSpec) -> bool {
        // `is-active --quiet` answers through its exit code alone.
        crate::io::exec(&Command::new(
            "systemctl",
            vec![
                "--user".into(),
                "is-active".into(),
                "--quiet".into(),
                spec.id.systemd.clone(),
            ],
        ))
        .is_ok()
    }
}

/// The unit text: the service's own command line, with the launch
/// environment read from the 0600 EnvironmentFile at every start,
/// systemd's own keep-alive (`Restart=on-failure`) backing the OS runner,
/// and a start-rate ceiling behind it: a process that crash-loops is
/// stopped after ten restarts in an hour instead of spinning forever.
fn unit_file(spec: &ServiceSpec) -> String {
    let exec = spec
        .unix_exec()
        .into_iter()
        .map(|arg| systemd_quote(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description={}\n\
         StartLimitIntervalSec=3600\n\
         StartLimitBurst=10\n\
         \n\
         [Service]\n\
         Type=simple\n\
         EnvironmentFile={}\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        spec.display_name,
        systemd_quote_value(&spec.env_file.to_string_lossy())
    )
}

/// Quote one ExecStart word for systemd: a space inside the word needs
/// the whole word quoted, and `%`/`$` are systemd's own specifier and
/// variable escapes — a literal dollar in a path must not expand at
/// activation.
pub(crate) fn systemd_quote(word: &str) -> String {
    if word.is_empty() {
        return "\"\"".to_string();
    }
    if !word
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '%' || c == '$' || c == '\\')
    {
        return word.to_string();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        match c {
            '%' | '$' | '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Quote a value position (not a word): EnvironmentFile paths are already
/// one value with no separators to protect, but a stray quote or percent
/// in a home directory must survive systemd's parser unchanged.
fn systemd_quote_value(value: &str) -> String {
    if !value.chars().any(|c| matches!(c, '"' | '%' | '$' | '\\')) {
        return value.to_string();
    }
    systemd_quote(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::UnitId;
    use std::path::Path;

    fn spec(home: &Path) -> ServiceSpec {
        ServiceSpec {
            id: UnitId {
                systemd: "mydoor.service".into(),
                launchd: "com.example.mydoor".into(),
                windows: "My Door".into(),
            },
            display_name: "My door endpoint".into(),
            binary: home.join(".local/bin/mydoor"),
            args: vec![
                "--store".into(),
                "/data/my vault.store".into(),
                "--http".into(),
                "8765".into(),
                "--allow-write".into(),
            ],
            unix_args: vec!["--no-keystore".into()],
            windows_args: vec!["--tray".into()],
            windows_env_drop: vec![],
            env_file: home.join("data/launch.env"),
            data_dir: home.join("data"),
        }
    }

    #[test]
    fn the_unit_names_the_binary_and_holds_no_secret() {
        let home = Path::new("/fake-home");
        let runner = SystemdUserRunner::at(home);
        let s = spec(home);
        let files = runner.install_files(&s, &[("MYDOOR_TOKEN".into(), "deadbeef".into())]);
        assert_eq!(files.len(), 1, "systemd gets one artifact: the unit");
        assert_eq!(
            files[0].path,
            home.join(".config/systemd/user/mydoor.service")
        );
        assert!(!files[0].secret, "the unit is not the secret");
        let unit = &files[0].contents;
        assert!(
            unit.contains("EnvironmentFile=/fake-home/data/launch.env"),
            "the environment rides the 0600 file, named by path: {unit}"
        );
        assert!(
            !unit.contains("deadbeef") && !unit.contains("MYDOOR_TOKEN"),
            "no value of any kind in a world-readable unit: {unit}"
        );
        assert!(
            unit.contains("Restart=on-failure"),
            "the runner keeps the process up"
        );
        assert!(
            unit.contains("WantedBy=default.target"),
            "login-time start is the promise"
        );
        assert!(
            unit.contains("StartLimitIntervalSec=3600"),
            "start-rate ceiling: {unit}"
        );
        assert!(unit.contains("StartLimitBurst=10"));
        assert!(
            unit.find("StartLimitIntervalSec").unwrap() < unit.find("[Service]").unwrap(),
            "the limits sit in [Unit], where systemd reads them"
        );
        // The ExecStart line: the binary path and every argument, each
        // space-bearing word quoted for systemd's parser.
        assert!(unit.contains("/fake-home/.local/bin/mydoor --store \"/data/my vault.store\""));
        assert!(unit.contains("--http 8765") && unit.contains("--allow-write"));
        assert!(
            unit.contains("--no-keystore"),
            "unix extras reach the unix unit"
        );
        assert!(
            !unit.contains("--tray"),
            "windows extras stay off the unix unit"
        );
        assert!(
            !unit.contains("--parent-pid"),
            "a service outlives whoever installed it"
        );
    }

    #[test]
    fn systemd_quotes_survive_the_parser() {
        // A space-bearing path is quoted as one word; a plain word stays
        // bare; `$`/`%` are doubled-escaped so systemd never expands them
        // at activation.
        assert_eq!(systemd_quote("plain"), "plain");
        assert_eq!(systemd_quote("a b"), "\"a b\"");
        assert_eq!(systemd_quote(""), "\"\"");
        assert_eq!(systemd_quote("/tmp/$HOME/%f"), "\"/tmp/\\$HOME/\\%f\"");
        assert_eq!(systemd_quote_value("/plain/path"), "/plain/path");
        assert_eq!(systemd_quote_value("/tmp/%h"), "\"/tmp/\\%h\"");
    }

    #[test]
    fn install_asks_for_linger_before_enable_and_names_the_known_user() {
        let runner = SystemdUserRunner::at("/h").with_linger_user("testuser");
        let s = spec(Path::new("/h"));
        let cmds = runner.install_cmds(&s);
        assert_eq!(cmds[0].program, "loginctl");
        assert_eq!(cmds[0].args, vec!["enable-linger", "testuser"]);
        assert!(
            cmds[0].lenient,
            "a logind that refuses must not fail the install"
        );
        assert_eq!(
            cmds[2].args,
            vec!["--user", "enable", "--now", "mydoor.service"]
        );
        // An unnamed runner asks bare: loginctl's own session-user spelling.
        let bare = SystemdUserRunner::at("/h");
        assert_eq!(
            bare.install_cmds(&spec(Path::new("/h")))[0].args,
            vec!["enable-linger"]
        );
    }
}
