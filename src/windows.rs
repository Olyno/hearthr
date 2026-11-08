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
//!
//! The drop list never excuses the wrapper's own grammar, though: `"`
//! and `%` cannot ride inside cmd's quotes verbatim (`"` ends the quote,
//! `%` is cmd's call/expansion grammar), and a `set` key outside
//! `[A-Za-z0-9_]` cannot be named at all.
//! [`OsServiceRunner::validate`] refuses those before an install
//! registers anything; a refused pair the caller still wants to pass
//! belongs to [`crate::ServiceSpec::windows_env_drop`]. A `&` inside a
//! quoted `set "K=V"` value stays literal to cmd, so values keep it.

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

    /// cmd's quoting cannot carry `"` or `%` verbatim inside a word (the
    /// quote ends the quoting, `%` is call/expansion grammar a later
    /// `cmd /c` re-reads), and a `set` name outside `[A-Za-z0-9_]` is no
    /// name at all — refuse such an install outright rather than write a
    /// wrapper that reinterprets its own content. Empty words are legal
    /// cmd syntax inside the quotes and pass. The pairs are checked
    /// after the drop filter: a name the caller already declared unsafe
    /// never reaches the wrapper, so it cannot fail the install.
    fn validate(&self, spec: &ServiceSpec, env: &[(String, String)]) -> Option<String> {
        // The wrapper path rides the task's own `cmd /c "<path>"` action
        // line: a percent in the data dir would expand there before the
        // wrapper is ever read.
        let dir = spec.data_dir.to_string_lossy();
        if dir.contains('%') {
            return Some(format!("data dir carries a percent: {dir}"));
        }
        for word in spec.windows_exec() {
            if word.contains('"') {
                return Some(format!("exec word carries a quote: {word}"));
            }
            if word.contains('%') {
                return Some(format!("exec word carries a percent: {word}"));
            }
        }
        for (key, value) in spec.windows_env(env) {
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Some(format!("env key is not a cmd name: {key:?}"));
            }
            if value.contains('%') || value.contains('"') {
                return Some(format!("env value for {key} carries % or a quote"));
            }
        }
        None
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

    /// Runtime state comes from PowerShell, not `schtasks`: `/Query /XML`
    /// carries no `<Status>` node at all, and `/Query /V /FO LIST`'s
    /// Status values are localized — a non-English machine's words are
    /// not parseable portably. `(Get-ScheduledTaskInfo ...).TaskState
    /// -ne 'Ready'` exits 0 exactly when the task is up; program+argv,
    /// no shell. Where `powershell.exe` is not on PATH the old `/Query
    /// /XML` `<Status>` heuristic stays: a machine without PowerShell
    /// keeps the behavior it had before, and a missing probe tool is
    /// never a startup failure.
    fn running(&self, spec: &ServiceSpec) -> bool {
        if crate::runner::tool_on_path("powershell") {
            // Exit code only: exit 0 says the state is not Ready, so it
            // is running.
            return crate::io::exec(&self.running_cmd(spec)).is_ok();
        }
        crate::io::exec(&self.schtasks(spec, "/Query", &["/XML"]))
            .is_ok_and(|text| text.contains("<Status>Running</Status>"))
    }
}

impl WindowsTaskRunner {
    /// The PowerShell probe behind [`WindowsTaskRunner::running`]: pure
    /// argv construction, one single-quoted `-Command` whose task name
    /// doubles every `'` per PowerShell's single-quoted-string grammar —
    /// the name is free text, and a stray quote there must not open a
    /// new expression.
    pub(crate) fn running_cmd(&self, spec: &ServiceSpec) -> Command {
        let quoted = spec.id.windows.replace('\'', "''");
        Command::new(
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!("(Get-ScheduledTaskInfo -TaskName '{quoted}').TaskState -ne 'Ready'"),
            ],
        )
    }
}

/// The wrapper: `@echo off`, one quoted `set` per pair, then the binary
/// with every argument quoted. Values and words ride inside double
/// quotes, where cmd keeps metacharacters literal (`set "NAME=VALUE"` is
/// the injection-safe form: the line ends at the value's own closing
/// quote, not at the first `&`; an unquoted `x&whoami` arg is not). The
/// spec's drop list has already filtered the pairs, and [`OsServiceRunner::validate`]
/// has already refused the `"`/`%` the quotes cannot carry.
fn wrapper_cmd(spec: &ServiceSpec, env: &[(String, String)]) -> String {
    let mut out = String::from("@echo off\r\n");
    for (key, value) in env {
        out.push_str(&format!("set \"{}={}\" \r\n", key, value));
    }
    // Every exec word is one quoted word — the binary path (APPDATA
    // paths carry spaces by construction) and every argument, so no
    // metacharacter in a value can reach cmd as grammar.
    let args = spec
        .windows_exec()
        .into_iter()
        .skip(1)
        .map(|arg| format!("\"{arg}\""))
        .collect::<Vec<_>>()
        .join(" ");
    out.push_str(&format!(
        "\"{}\" {}\r\n",
        spec.binary.to_string_lossy(),
        args
    ));
    out
}

/// The task XML: a logon trigger, `cmd.exe /d /c` on the wrapper, and
/// scheduler-side restart-on-failure backing the same `Restart=on-failure`
/// the systemd unit asks for. `ExecutionTimeLimit PT0S` says the service
/// is never killed for being long-running. The wrapper path is XML-
/// escaped: it lives in the owner's app-data dir, and a directory name
/// with `&` would otherwise end the `<Arguments>` text node.
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
        wrapper = xml_escape(&wrapper.to_string_lossy())
    )
}

/// Escape a string for XML text. Kept independent of the launchd
/// adapter's identical escape: two adapters sharing a util would couple
/// two grammars that have no reason to move together.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
            wrapper.contains("\"C:\\Users\\me Me\\AppData\\Local\\mydoor.exe\" \"--http\" \"8765\" \"--store\" \"C:\\data\\my vault.store\" \"--tray\""),
            "every exec word quoted, windows extras only: {wrapper}"
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
    fn every_exec_word_is_quoted_so_a_metacharacter_stays_data() {
        let s = ServiceSpec {
            args: vec!["x&echo_INJECTED".into(), "plain".into()],
            ..spec(Path::new("C:\\d"))
        };
        let wrapper = wrapper_cmd(&s, &[]);
        let exec_line = wrapper.lines().last().unwrap();
        assert!(
            exec_line.contains("\"x&echo_INJECTED\""),
            "the ampersand rides inside quotes: {exec_line}"
        );
        assert!(
            exec_line.contains("\"plain\""),
            "a space-free word is quoted too: {exec_line}"
        );
        // Outside the quotes there is no bare `&`: the only unquoted
        // text is the quoting grammar itself.
        let naked: String = {
            let mut out = String::new();
            let mut quoted = false;
            for c in exec_line.chars() {
                match c {
                    '"' => quoted = !quoted,
                    '&' if !quoted => out.push(c),
                    _ => {}
                }
            }
            out
        };
        assert!(naked.is_empty(), "unquoted metacharacters: {naked}");
    }

    #[test]
    fn validate_refuses_what_cmd_cannot_carry() {
        let runner = WindowsTaskRunner;
        let base = spec(Path::new("C:\\d"));
        let with_args = |args: Vec<String>| ServiceSpec {
            args,
            ..base.clone()
        };
        assert!(
            runner
                .validate(&base, &[("K".into(), "a&b".into())])
                .is_none(),
            "& inside a quoted value stays literal to cmd"
        );
        assert!(
            runner
                .validate(&with_args(vec!["%USERPROFILE%".into()]), &[])
                .is_some(),
            "cmd's expansion grammar cannot ride verbatim"
        );
        assert!(
            runner
                .validate(&with_args(vec!["he\"llo".into()]), &[])
                .is_some(),
            "a quote would end its own word"
        );
        assert!(
            runner
                .validate(&base, &[("K".into(), "100%".into())])
                .is_some(),
            "a % in a value is the same grammar"
        );
        assert!(
            runner
                .validate(&base, &[("K".into(), "a\"b".into())])
                .is_some(),
            "a quote in a value ends the set early"
        );
        assert!(
            runner
                .validate(&base, &[("bad key".into(), "v".into())])
                .is_some(),
            "not a cmd name"
        );
        assert!(
            runner.validate(&base, &[("".into(), "v".into())]).is_some(),
            "no name at all"
        );
        assert!(
            runner.validate(&with_args(vec!["".into()]), &[]).is_none(),
            "an empty word is legal cmd syntax inside quotes"
        );
        assert!(
            runner
                .validate(&base, &[("MYDOOR_JSON".into(), "100%".into())])
                .is_none(),
            "a dropped name never reaches the wrapper, so it cannot fail the install"
        );
    }

    #[test]
    fn the_wrapper_path_is_escaped_in_the_task_xml() {
        let xml = task_xml(Path::new("C:\\a&b\\my<door>\"x\"\\mydoor.cmd"));
        assert!(xml.contains("&amp;"), "the ampersand is an entity");
        let arguments = xml
            .split("<Arguments>")
            .nth(1)
            .unwrap()
            .split("</Arguments>")
            .next()
            .unwrap();
        assert!(arguments.contains("&amp;"));
        assert!(arguments.contains("&lt;") && arguments.contains("&gt;"));
        // Every entity becomes one sentinel char; what is left must hold
        // no XML metacharacter raw — entities only.
        let stripped = arguments
            .replace("&amp;", "\u{1}")
            .replace("&lt;", "\u{1}")
            .replace("&gt;", "\u{1}")
            .replace("&quot;", "\u{1}");
        assert!(
            !stripped.contains('&')
                && !stripped.contains('<')
                && !stripped.contains('>')
                && !stripped.contains('"'),
            "raw metacharacter left to break the node: {arguments}"
        );
        assert!(
            arguments.starts_with("/d /c &quot;") && arguments.ends_with("&quot;"),
            "cmd.exe /d /c on the quoted path stays the structure"
        );
    }

    #[test]
    fn xml_escape_holds_the_grammar() {
        assert_eq!(xml_escape("a&b<c>d\"e"), "a&amp;b&lt;c&gt;d&quot;e");
    }

    #[test]
    fn the_running_probe_doubles_single_quotes_in_the_task_name() {
        let runner = WindowsTaskRunner;
        let s = ServiceSpec {
            id: UnitId {
                systemd: "x.service".into(),
                launchd: "x".into(),
                windows: "Bob's Door".into(),
            },
            ..spec(Path::new("C:\\d"))
        };
        let cmd = runner.running_cmd(&s);
        assert_eq!(cmd.program, "powershell.exe");
        assert_eq!(cmd.args[0], "-NoProfile");
        assert_eq!(cmd.args[1], "-NonInteractive");
        assert_eq!(cmd.args[2], "-Command");
        assert_eq!(
            cmd.args[3], "(Get-ScheduledTaskInfo -TaskName 'Bob''s Door').TaskState -ne 'Ready'",
            "the name doubles its quote; the grammar quotes stay one pair"
        );
    }
}
