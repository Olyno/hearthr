//! macOS: a launchd LaunchAgent under `~/Library/LaunchAgents/`, driven
//! by `launchctl bootstrap`/`bootout`. `RunAtLoad` starts the process at
//! login and `KeepAlive` brings it back after a crash: the crate's
//! keep-alive law names launchd as the relauncher, and `ThrottleInterval`
//! is what bounds the loop — a crash loop may not spin faster than one
//! respawn every ten seconds. Stop therefore answers with a `bootout`,
//! not a kill: a KeepAlive job that is merely killed is respawned, so a
//! stopped job is an unloaded job.
//!
//! The launch environment rides the plist's `EnvironmentVariables` dict,
//! and the plist is written `0600`: the trust boundary is the owner's own
//! login session — a value readable only by this user guards nothing
//! this user could not already take.
//!
//! The launchd target domain is `gui/<uid>`. The uid is a field like the
//! home, never a live probe inside generation: tests inject one, and the
//! default asks `id -u` — the tool every launchd recipe interpolates —
//! once, directly, because no command in this crate goes through a shell.
//! A machine where `id` cannot answer gets 0, which makes every call fail
//! with launchd's own words rather than inventing a domain that might own
//! someone else's session.

use std::path::PathBuf;

use crate::runner::OsServiceRunner;
use crate::spec::{Command, ServiceFile, ServiceSpec};

pub struct LaunchAgentRunner {
    /// The home whose `Library/LaunchAgents` holds the plist. Tests
    /// inject a fixed one so generation is deterministic.
    home: PathBuf,
    /// The numeric uid of the user whose `gui/` domain owns the job.
    uid: u32,
}

impl LaunchAgentRunner {
    /// The runner writing the LaunchAgent under `home` for `uid`.
    pub fn at(home: impl Into<PathBuf>, uid: u32) -> Self {
        Self {
            home: home.into(),
            uid,
        }
    }

    /// The plist path for this spec.
    pub fn plist_path(&self, spec: &ServiceSpec) -> PathBuf {
        self.home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", spec.id.launchd))
    }

    /// The `gui/<uid>` service target every modern launchctl call speaks
    /// to.
    fn target(&self) -> String {
        format!("gui/{}", self.uid)
    }

    /// The fully-qualified job target: `gui/501/com.example.mydoor`.
    fn job(&self, spec: &ServiceSpec) -> String {
        format!("{}/{}", self.target(), spec.id.launchd)
    }
}

impl Default for LaunchAgentRunner {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::at(home, current_uid())
    }
}

impl OsServiceRunner for LaunchAgentRunner {
    fn name(&self) -> &'static str {
        "launchd"
    }

    /// The plist on disk is installed-ness — the same cheap file check
    /// the systemd adapter gets, no `launchctl print` needed.
    fn installed(&self, spec: &ServiceSpec) -> bool {
        self.plist_path(spec).is_file()
    }

    fn install_files(&self, spec: &ServiceSpec, env: &[(String, String)]) -> Vec<ServiceFile> {
        vec![ServiceFile {
            path: self.plist_path(spec),
            contents: plist_file(spec, env),
            // The EnvironmentVariables dict carries the launch
            // environment: owner-only.
            secret: true,
        }]
    }

    fn install_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![
            // bootout first, leniently: re-bootstrapping a live label is
            // exactly the error the user's "install again" click must not
            // pay. enable clears a `disabled` flag a previous bootout
            // left on the label (launchd persists disabled jobs across
            // logins).
            Command::lenient("launchctl", vec!["bootout".into(), self.job(spec)]),
            Command::new("launchctl", vec!["enable".into(), self.job(spec)]),
            // Bootstrap runs a RunAtLoad job at once: the enable-and-start
            // a systemd install gets from `--now`.
            Command::new(
                "launchctl",
                vec![
                    "bootstrap".into(),
                    self.target(),
                    self.plist_path(spec).display().to_string(),
                ],
            ),
        ]
    }

    fn uninstall_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![Command::lenient(
            "launchctl",
            vec!["bootout".into(), self.job(spec)],
        )]
    }

    fn uninstall_files(&self, spec: &ServiceSpec) -> Vec<PathBuf> {
        vec![self.plist_path(spec)]
    }

    fn start_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        // A stop bootout can leave the label flagged disabled, and a
        // disabled label kickstarts to nothing. enable is strict here for
        // the same reason install_cmds treats it strictly: the start was
        // not delivered if the label cannot run. kickstart then starts
        // the loaded job (KeepAlive, not RunAtLoad, is what keeps it up).
        vec![
            Command::new("launchctl", vec!["enable".into(), self.job(spec)]),
            Command::new("launchctl", vec!["kickstart".into(), self.job(spec)]),
        ]
    }

    fn stop_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        // The graceful ask first, then the unload: a KeepAlive job that
        // is only killed is answered with a respawn, so the stop is not
        // real until the job is booted out. Both lenient: a graceful
        // service that exited on the SIGTERM before bootout ran, and a
        // bootout of an already-booted-out job, are clean slates, not
        // failures.
        vec![
            Command::lenient(
                "launchctl",
                vec!["kill".into(), "SIGTERM".into(), self.job(spec)],
            ),
            Command::lenient("launchctl", vec!["bootout".into(), self.job(spec)]),
        ]
    }

    fn restart_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        // The plist IS the environment: replace the job with itself,
        // enable included — bootout can leave the label disabled, and a
        // disabled bootstrap would start nothing.
        vec![
            Command::lenient("launchctl", vec!["bootout".into(), self.job(spec)]),
            Command::new("launchctl", vec!["enable".into(), self.job(spec)]),
            Command::new(
                "launchctl",
                vec![
                    "bootstrap".into(),
                    self.target(),
                    self.plist_path(spec).display().to_string(),
                ],
            ),
        ]
    }

    fn running(&self, spec: &ServiceSpec) -> bool {
        // `launchctl print` carries a `state = running` line for a loaded
        // job that is up; a job that is not there exits non-zero.
        match crate::io::exec(&Command::new(
            "launchctl",
            vec!["print".into(), self.job(spec)],
        )) {
            Ok(out) => out.contains("state = running"),
            Err(_) => false,
        }
    }
}

/// The LaunchAgent plist: label, the service's own argv (no shell string
/// — each argument is one `<string>` node, XML-escaped only), the launch
/// environment in the `EnvironmentVariables` dict of this `0600` file,
/// run-at-login plus keep-alive, and a throttle so the crash-restart
/// respawn cannot spin the CPU — a crash loop runs at most one restart
/// every `ThrottleInterval` seconds. stderr goes to a log file in the
/// data dir: a daemon nobody reads from must still tell someone why it
/// died, and launchd has no terminal to tell them in.
fn plist_file(spec: &ServiceSpec, env: &[(String, String)]) -> String {
    let args = spec
        .unix_exec()
        .into_iter()
        .map(|arg| format!("        <string>{}</string>", xml_escape(&arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let variables = env
        .iter()
        .map(|(key, value)| {
            format!(
                "        <key>{}</key>\n        <string>{}</string>",
                xml_escape(key),
                xml_escape(value)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
{}
    </array>
    <key>EnvironmentVariables</key>
    <dict>
{}
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&spec.id.launchd),
        args,
        variables,
        xml_escape(&spec.log_path().to_string_lossy())
    )
}

/// This user's uid, for the `gui/<uid>` domain, asked of `id -u` directly
/// (see the module doc for why not through a shell).
fn current_uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Escape a string for XML text.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
            args: vec!["--http".into(), "8765".into()],
            unix_args: vec!["--no-keystore".into()],
            windows_args: vec!["--tray".into()],
            windows_env_drop: vec![],
            env_file: home.join("data/launch.env"),
            data_dir: home.join("data"),
        }
    }

    #[test]
    fn the_plist_is_the_secret_and_carries_the_environment_inline() {
        let home = Path::new("/Users/me");
        let runner = LaunchAgentRunner::at(home, 501);
        let s = spec(home);
        let files = runner.install_files(&s, &[("MYDOOR_TOKEN".into(), "hex&<>value".into())]);
        assert_eq!(files.len(), 1);
        assert!(files[0].secret, "the plist carries the env: owner-only");
        assert_eq!(
            files[0].path,
            home.join("Library/LaunchAgents/com.example.mydoor.plist")
        );
        let plist = &files[0].contents;
        assert!(plist.contains("<string>com.example.mydoor</string>"));
        // launchd's dict keeps every pair (unix grammar): the value here,
        // XML-escaped, never shell-expanded.
        assert!(plist.contains("<key>MYDOOR_TOKEN</key>"));
        assert!(
            plist.contains("hex&amp;&lt;&gt;value"),
            "escaped, not injected"
        );
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(
            plist.contains("<key>KeepAlive</key>\n    <true/>"),
            "a crash restarts: the throttle, not the absence of keep-alive, bounds the loop"
        );
        assert!(
            plist.contains("--no-keystore"),
            "unix extras reach the plist"
        );
        assert!(!plist.contains("--tray"));
        assert!(
            plist.contains("<string>/Users/me/data/mydoor.log</string>"),
            "stderr lands in the data dir"
        );
    }

    #[test]
    fn the_job_verbs_name_the_uid_domain() {
        let runner = LaunchAgentRunner::at("/Users/me", 501);
        let s = spec(Path::new("/Users/me"));
        let install = runner.install_cmds(&s);
        assert_eq!(
            install[0].args,
            vec!["bootout", "gui/501/com.example.mydoor"]
        );
        assert!(
            install[0].lenient,
            "re-install must not pay the already-loaded error"
        );
        assert_eq!(install[2].args[0], "bootstrap");
        assert_eq!(install[2].args[1], "gui/501");
        // Stop unloads: a killed KeepAlive job comes back, a booted-out
        // one does not.
        let stop = runner.stop_cmds(&s);
        assert_eq!(
            stop[0].args,
            vec!["kill", "SIGTERM", "gui/501/com.example.mydoor"]
        );
        assert_eq!(stop[1].args, vec!["bootout", "gui/501/com.example.mydoor"]);
        // Restart replaces the job — the plist IS the environment.
        let restart = runner.restart_cmds(&s);
        assert_eq!(restart.len(), 3);
        assert_eq!(restart[0].args[0], "bootout");
        assert_eq!(restart[1].args[0], "enable");
        assert_eq!(restart[2].args[0], "bootstrap");
    }

    #[test]
    fn the_plist_keeps_a_crashed_job_alive() {
        let runner = LaunchAgentRunner::at("/Users/me", 501);
        let files = runner.install_files(&spec(Path::new("/Users/me")), &[]);
        let plist = &files[0].contents;
        assert!(
            plist.contains("<key>KeepAlive</key>\n    <true/>"),
            "the keep-alive law names launchd as the relauncher"
        );
        assert!(plist.contains("<key>ThrottleInterval</key>\n    <integer>10</integer>"));
    }

    #[test]
    fn stop_unloads_and_start_re_enables() {
        let runner = LaunchAgentRunner::at("/Users/me", 501);
        let s = spec(Path::new("/Users/me"));
        let stop = runner.stop_cmds(&s);
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0].args,
            vec!["kill", "SIGTERM", "gui/501/com.example.mydoor"]
        );
        assert!(
            stop[0].lenient,
            "a job that died on the SIGTERM is stopped already"
        );
        assert_eq!(stop[1].args, vec!["bootout", "gui/501/com.example.mydoor"]);
        assert!(
            stop[1].lenient,
            "booting out a stopped job is the clean slate"
        );
        let start = runner.start_cmds(&s);
        assert_eq!(start.len(), 2);
        assert_eq!(start[0].args, vec!["enable", "gui/501/com.example.mydoor"]);
        assert!(
            !start[0].lenient,
            "a label that cannot run means the start was not delivered"
        );
        assert_eq!(
            start[1].args,
            vec!["kickstart", "gui/501/com.example.mydoor"]
        );
        assert!(!start[1].lenient);
        let restart = runner.restart_cmds(&s);
        assert!(
            restart[0].lenient,
            "bootout of a gone job is the clean slate"
        );
        assert!(!restart[1].lenient);
        assert!(!restart[2].lenient);
    }

    #[test]
    fn xml_escape_holds_the_grammar() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }
}
