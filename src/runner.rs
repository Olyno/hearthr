use std::path::{Path, PathBuf};

use crate::spec::{Command, ServiceFile, ServiceSpec};

/// The platform adapter: every OS keeps a background process through its
/// own runner, and this trait is the whole seam between the lifecycle
/// cores and that difference. Generation is pure — [`OsServiceRunner::install_files`]
/// and the command lists never touch the machine; only [`OsServiceRunner::installed`]
/// and [`OsServiceRunner::running`] observe it, and only cheaply.
pub trait OsServiceRunner: Send + Sync {
    /// Stable id on the wire (matches [`crate::ServiceState::kind`]).
    fn name(&self) -> &'static str;
    /// Whether the artifact is registered, without executing anything:
    /// every platform keeps its service in a file this crate can see.
    fn installed(&self, spec: &ServiceSpec) -> bool;
    /// The files an install writes, in order. Pure.
    fn install_files(&self, spec: &ServiceSpec, env: &[(String, String)]) -> Vec<ServiceFile>;
    /// The commands that finish the registration (reload, enable,
    /// bootstrap), run after the files exist. Pure.
    fn install_cmds(&self, spec: &ServiceSpec) -> Vec<Command>;
    /// Preflight refusal on the content the install carries, for
    /// adapters whose artifact goes through a command interpreter: an
    /// exec word or environment pair that the interpreter cannot carry
    /// verbatim is refused here — surfaced as an install failure before
    /// anything is registered — instead of written into a file the shell
    /// would reinterpret. Pure; [`None`] accepts.
    fn validate(&self, _spec: &ServiceSpec, _env: &[(String, String)]) -> Option<String> {
        None
    }
    /// The commands that unregister, and the files to remove with them.
    fn uninstall_cmds(&self, spec: &ServiceSpec) -> Vec<Command>;
    fn uninstall_files(&self, spec: &ServiceSpec) -> Vec<PathBuf>;
    fn start_cmds(&self, spec: &ServiceSpec) -> Vec<Command>;
    fn stop_cmds(&self, spec: &ServiceSpec) -> Vec<Command>;
    /// Commands that make the running endpoint re-read its environment.
    /// systemd restarts the unit; launchd replaces the job (its plist is
    /// the environment); schtasks ends and runs the task. Pure.
    fn restart_cmds(&self, spec: &ServiceSpec) -> Vec<Command>;
    /// Whether the OS says the unit is up right now — executes its status
    /// probe, so it is only ever asked while installed.
    fn running(&self, spec: &ServiceSpec) -> bool;
}

/// The production probe behind [`detect`]: is `tool` an executable on
/// this process's `PATH`?
pub(crate) fn tool_on_path(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(tool);
        candidate.is_file()
            || (cfg!(windows)
                && (candidate.with_extension("exe").is_file()
                    || candidate.with_extension("cmd").is_file()))
    })
}

/// Pick the adapter for THIS machine at runtime: the platform's own
/// runner wins when the tool it drives is actually present, and a machine
/// without one gets `None` — the caller keeps supervising its child,
/// which is every install's default posture.
pub fn detect(
    on_path: impl Fn(&str) -> bool,
    path_exists: impl Fn(&Path) -> bool,
) -> Option<Box<dyn OsServiceRunner>> {
    detect_os(std::env::consts::OS, &on_path, &path_exists)
}

/// [`detect`] with the OS named: the whole selection rule is one match
/// over strings, so every branch is testable on any machine.
pub fn detect_os(
    os: &str,
    on_path: &dyn Fn(&str) -> bool,
    path_exists: &dyn Fn(&Path) -> bool,
) -> Option<Box<dyn OsServiceRunner>> {
    match os {
        // systemd user units need both the tool and a systemd session to
        // talk to (the canonical `/run/systemd/system` marker).
        "linux" if on_path("systemctl") && path_exists(Path::new("/run/systemd/system")) => {
            Some(Box::new(crate::systemd::SystemdUserRunner::default()))
        }
        "macos" if on_path("launchctl") => {
            Some(Box::new(crate::launchd::LaunchAgentRunner::default()))
        }
        "windows" if on_path("schtasks") => Some(Box::new(crate::windows::WindowsTaskRunner)),
        _ => None,
    }
}

/// The live adapter for lifecycle callers.
pub(crate) fn runner() -> Option<Box<dyn OsServiceRunner>> {
    detect(tool_on_path, |path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::UnitId;
    use std::path::PathBuf;

    fn probe_spec() -> ServiceSpec {
        ServiceSpec {
            id: UnitId {
                systemd: "probe.service".into(),
                launchd: "com.example.probe".into(),
                windows: "Probe".into(),
            },
            display_name: "Probe".into(),
            binary: PathBuf::from("/bin/true"),
            args: vec![],
            unix_args: vec![],
            windows_args: vec![],
            windows_env_drop: vec![],
            env_file: PathBuf::from("/tmp/probe/launch.env"),
            data_dir: PathBuf::from("/tmp/probe"),
        }
    }

    #[test]
    fn the_runner_rule_wants_the_platform_tool_and_on_linux_a_systemd_session() {
        let yes = |_tool: &str| true;
        let no = |_tool: &str| false;
        let marker = |_p: &Path| true;
        let gone = |_p: &Path| false;

        assert_eq!(
            detect_os("linux", &yes, &marker).unwrap().name(),
            "systemd-user"
        );
        assert!(
            detect_os("linux", &yes, &gone).is_none(),
            "no systemd session, no unit"
        );
        assert!(
            detect_os("linux", &no, &marker).is_none(),
            "no systemctl, no adapter"
        );
        assert_eq!(detect_os("macos", &yes, &marker).unwrap().name(), "launchd");
        assert!(detect_os("macos", &no, &marker).is_none());
        assert_eq!(
            detect_os("windows", &yes, &marker).unwrap().name(),
            "schtasks"
        );
        assert!(detect_os("windows", &no, &marker).is_none());
        assert!(
            detect_os("haiku", &yes, &marker).is_none(),
            "unknown OS, no adapter, the caller's child stays home"
        );
    }

    #[test]
    fn installed_is_the_artifact_file_on_every_platform() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let spec = probe_spec();
        let spec = ServiceSpec {
            env_file: home.join("launch.env"),
            data_dir: home.join("data"),
            ..spec.clone()
        };
        let systemd = crate::systemd::SystemdUserRunner::at(home);
        let launchd = crate::launchd::LaunchAgentRunner::at(home, 501);
        let windows = crate::windows::WindowsTaskRunner;
        assert!(
            !systemd.installed(&spec) && !launchd.installed(&spec) && !windows.installed(&spec)
        );
        std::fs::create_dir_all(systemd.unit_path(&spec).parent().unwrap()).unwrap();
        std::fs::write(systemd.unit_path(&spec), "[Unit]").unwrap();
        assert!(
            systemd.installed(&spec),
            "the unit file IS the registration"
        );
        std::fs::create_dir_all(launchd.plist_path(&spec).parent().unwrap()).unwrap();
        std::fs::write(launchd.plist_path(&spec), "<plist>").unwrap();
        assert!(launchd.installed(&spec));
        std::fs::create_dir_all(home.join("data")).unwrap();
        std::fs::write(windows.task_xml_path(&spec), "<Task>").unwrap();
        assert!(windows.installed(&spec));
    }
}
