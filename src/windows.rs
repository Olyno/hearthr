//! Windows: Task Scheduler, driven by `schtasks`. The artifact is a task
//! XML registered with a logon trigger (`schtasks /Create /XML` on the
//! creating user's own logon task needs no elevation), and the command
//! line runs through a generated wrapper `.cmd` that `set`s the launch
//! environment and execs the binary — cmd carries no other way to put an
//! environment on a task's child.
//!
//! Trust boundary: the task runs as the owner, the wrapper and the XML
//! live under the owner's app-data dir, and the wrapper is written
//! owner-only where the platform enforces it; the secrecy of a pair that
//! reaches the wrapper is the per-user APPDATA directory's ACL.
//!
//! The spec's [`crate::ServiceSpec::windows_env_drop`] names the
//! environment entries whose values cmd's quoting cannot promise to carry
//! verbatim (a bare `&` inside a JSON value is one) — the wrapper
//! renderer applies the filter, so a value the caller declares unsafe
//! never reaches the task environment.

use std::path::{Path, PathBuf};

use crate::runner::OsServiceRunner;
use crate::spec::{Command, ServiceFile, ServiceSpec};
/// The Task Scheduler adapter is stateless: every path it owns derives
/// from the spec's data dir, so one value serves every install.
pub struct WindowsTaskRunner;

impl WindowsTaskRunner {
    pub(crate) fn task_xml_path(&self, spec: &ServiceSpec) -> PathBuf {
        spec.data_dir
            .join(format!("{}.xml", spec.id.windows.replace(' ', "")))
    }

    /// The wrapper the task runs: sets the launch environment and execs
    /// the binary. It carries environment values, so it is written
    /// owner-only.
    pub(crate) fn wrapper_path(&self, spec: &ServiceSpec) -> PathBuf {
        spec.data_dir.join(format!(
            "{}.cmd",
            spec.id.launchd.rsplit('.').next().unwrap_or("service")
        ))
    }

    /// One `schtasks` call over this task's name: the verb flags keep one
    /// argv grammar across create/delete/run/end/query.
    fn schtasks(&self, spec: &ServiceSpec, verb: &str, extra: &[&str]) -> Command {
        let mut args = vec![verb.to_string(), "/TN".to_string(), spec.id.windows.clone()];
        args.extend(extra.iter().map(|s| s.to_string()));
        Command::new("schtasks", args)
    }
}

impl OsServiceRunner for WindowsTaskRunner {
    fn name(&self) -> &'static str {
        "schtasks"
    }

    /// The XML artifact on disk is installed-ness for this adapter — the
    /// same cheap file check the other two get, and a task created by
    /// hand outside the app is not this app's service.
    fn installed(&self, spec: &ServiceSpec) -> bool {
        self.task_xml_path(spec).is_file()
    }

    fn install_files(&self, spec: &ServiceSpec, env: &[(String, String)]) -> Vec<ServiceFile> {
        let env = spec.windows_env(env);
        vec![
            ServiceFile {
                path: self.wrapper_path(spec),
                contents: wrapper_cmd(spec, &env),
                secret: true,
            },
            ServiceFile {
                path: self.task_xml_path(spec),
                contents: task_xml(&self.wrapper_path(spec)),
                secret: true,
            },
        ]
    }

    fn install_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        // /F re-registers this app's own task: installs are idempotent,
        // the same law systemd's `enable` keeps. The first create of a
        // machine that never had the task has nothing to end; /End before
        // /Create takes the old registration's running process out of the
        // way so the fresh one starts clean.
        vec![
            Command::lenient(
                "schtasks",
                vec!["/End".into(), "/TN".into(), spec.id.windows.clone()],
            ),
            self.schtasks(
                spec,
                "/Create",
                &["/XML", &self.task_xml_path(spec).to_string_lossy(), "/F"],
            ),
            // Start it once, now: login-time is the enable half, /Run is
            // the --now half.
            self.schtasks(spec, "/Run", &[]),
        ]
    }

    fn uninstall_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![
            Command::lenient(
                "schtasks",
                vec!["/End".into(), "/TN".into(), spec.id.windows.clone()],
            ),
            self.schtasks(spec, "/Delete", &["/F"]),
        ]
    }

    fn uninstall_files(&self, spec: &ServiceSpec) -> Vec<PathBuf> {
        vec![self.task_xml_path(spec), self.wrapper_path(spec)]
    }

    fn start_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![self.schtasks(spec, "/Run", &[])]
    }

    fn stop_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![self.schtasks(spec, "/End", &[])]
    }

    fn restart_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
        vec![
            Command::lenient(
                "schtasks",
                vec!["/End".into(), "/TN".into(), spec.id.windows.clone()],
            ),
            self.schtasks(spec, "/Run", &[]),
        ]
    }

    fn running(&self, spec: &ServiceSpec) -> bool {
        // `/Query /XML` answers with the task's own `<Status>` node — a
        // schema value, unlike the localized plain-text table.
        let out = crate::io::exec(&self.schtasks(spec, "/Query", &["/XML"]));
        out.is_ok_and(|text| text.contains("<Status>Running</Status>"))
    }
}

/// The wrapper: `@echo off`, one quoted `set` per pair, then the binary
/// with its arguments. Values ride inside double quotes, where cmd keeps
/// metacharacters literal (`set "NAME=VALUE"` is the injection-safe form:
/// the line ends at the value's own closing quote, not at the first `&`).
/// The spec's drop list has already filtered the pairs.
fn wrapper_cmd(spec: &ServiceSpec, env: &[(String, String)]) -> String {
    let mut out = String::from("@echo off\r\n");
    for (key, value) in env {
        out.push_str(&format!("set \"{}={}\" \r\n", key, value));
    }
    // A value with a quote would end the `set` early; those belong to the
    // drop list. The exec line quotes the binary path itself (APPDATA
    // paths carry spaces by construction).
    let args = spec
        .windows_exec()
        .into_iter()
        .skip(1)
        .map(|arg| cmd_quote_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    out.push_str(&format!(
        "\"{}\" {}\r\n",
        spec.binary.to_string_lossy(),
        args
    ));
    out
}

/// Quote one binary argument for the wrapper's last line: a path with a
/// space is one quoted word, anything else passes bare.
fn cmd_quote_arg(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// The task XML: a logon trigger, `cmd.exe /d /c` on the wrapper, and
/// scheduler-side restart-on-failure backing the same `Restart=on-failure`
/// the systemd unit asks for. `ExecutionTimeLimit PT0S` says the service
/// is never killed for being long-running.
fn task_xml(wrapper: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>cmd.exe</Command>
      <Arguments>/d /c &quot;{wrapper}&quot;</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        wrapper = wrapper.to_string_lossy()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::UnitId;
    use std::path::Path;

    fn spec(data: &Path) -> ServiceSpec {
        ServiceSpec {
            id: UnitId {
                systemd: "mydoor.service".into(),
                launchd: "com.example.mydoor".into(),
                windows: "My Door".into(),
            },
            display_name: "My door endpoint".into(),
            binary: PathBuf::from("C:\\Users\\me Me\\AppData\\Local\\mydoor.exe"),
            args: vec![
                "--http".into(),
                "8765".into(),
                "--store".into(),
                "C:\\data\\my vault.store".into(),
            ],
            unix_args: vec!["--no-keystore".into()],
            windows_args: vec!["--tray".into()],
            windows_env_drop: vec!["MYDOOR_JSON".into()],
            env_file: data.join("launch.env"),
            data_dir: data.to_path_buf(),
        }
    }

    #[test]
    fn the_wrapper_sets_the_env_and_execs_the_binary() {
        let s = spec(Path::new("C:\\Users\\me\\AppData\\Roaming\\mydoor"));
        let runner = WindowsTaskRunner;
        let files = runner.install_files(
            &s,
            &[
                ("MYDOOR_TOKEN".into(), "hex".into()),
                ("MYDOOR_JSON".into(), "{\"a\": 1}".into()), // dropped: cmd-unsafe
            ],
        );
        assert_eq!(files.len(), 2);
        assert!(
            files.iter().all(|f| f.secret),
            "both artifacts carry values"
        );
        let wrapper = &files[0].contents;
        assert!(wrapper.starts_with("@echo off\r\n"));
        assert!(wrapper.contains("set \"MYDOOR_TOKEN=hex\" \r\n"));
        assert!(!wrapper.contains("MYDOOR_JSON"), "the drop list honored");
        assert!(
            wrapper.contains("\"C:\\Users\\me Me\\AppData\\Local\\mydoor.exe\" --http 8765 --store \"C:\\data\\my vault.store\" --tray"),
            "binary quoted, space-bearing args quoted, windows extras only: {wrapper}"
        );
        assert!(
            !wrapper.contains("--no-keystore"),
            "unix extras stay off the wrapper"
        );
    }

    #[test]
    fn the_task_xml_carries_the_law_of_the_long_running_child() {
        let xml = task_xml(Path::new(
            "C:\\Users\\me\\AppData\\Roaming\\mydoor\\mydoor.cmd",
        ));
        assert!(
            xml.contains("<LogonTrigger>"),
            "login-time is the enable half"
        );
        assert!(
            xml.contains("<RestartOnFailure>"),
            "the scheduler keeps it up"
        );
        assert!(
            xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            "never killed for living long"
        );
        assert!(
            xml.contains("<RunLevel>LeastPrivilege</RunLevel>"),
            "no elevation claimed"
        );
        assert!(xml.contains("&quot;C:\\Users\\me\\AppData\\Roaming\\mydoor\\mydoor.cmd&quot;"));
    }

    #[test]
    fn verbs_keep_one_argv_grammar() {
        let s = spec(Path::new("C:\\d"));
        let runner = WindowsTaskRunner;
        let install = runner.install_cmds(&s);
        assert_eq!(install[0].args, vec!["/End", "/TN", "My Door"]);
        assert!(
            install[0].lenient,
            "ending a task that never ran is not an error"
        );
        assert_eq!(install[1].args[0], "/Create");
        assert_eq!(install[2].args, vec!["/Run", "/TN", "My Door"]);
        // Restart is end+run: the wrapper IS the environment.
        let restart = runner.restart_cmds(&s);
        assert_eq!(restart.len(), 2);
        assert_eq!(restart[1].args[0], "/Run");
    }

    #[test]
    fn cmd_quotes_only_spaces() {
        assert_eq!(cmd_quote_arg("plain"), "plain");
        assert_eq!(cmd_quote_arg("two words"), "\"two words\"");
    }
}
