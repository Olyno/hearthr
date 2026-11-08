use std::path::Path;

use crate::io::{env_to_text, remove_quiet, write_file, write_secret_file};
use crate::runner::OsServiceRunner;
use crate::spec::{Command, ServiceSpec, ServiceState};

/// The injectable executor every lifecycle core runs through: a borrowed
/// closure (not `'static`) so a test recorder can log through a local
/// sink, and [`crate::exec`] in production.
type Run<'a> = dyn FnMut(&Command) -> Result<String, String> + 'a;

/// Run a command sequence, tolerating the steps marked lenient (a clean
/// slate that was already clean), and keep the last non-empty output as
/// display detail — control commands occasionally answer loudly about a
/// step that still worked (systemd prints the job queue to stdout).
fn run_all(cmds: &[Command], run: &mut Run<'_>) -> Result<Option<String>, String> {
    let mut detail = None;
    for cmd in cmds {
        match run(cmd) {
            Ok(out) => {
                if !out.trim().is_empty() {
                    detail = Some(out.trim().to_string());
                }
            }
            // A lenient step failing is the clean slate it asked for: the
            // job was not loaded, the task was not running.
            Err(_) if cmd.lenient => {}
            Err(e) => return Err(format!("{}: {e}", cmd.display())),
        }
    }
    Ok(detail)
}

// --- The public lifecycle -----------------------------------------------------

/// What the background service is, right now: kind, unit, installed,
/// running — with `None` when this machine has no adapter at all (the
/// caller then keeps supervising its child, the default posture).
pub fn probe(spec: &ServiceSpec) -> Option<ServiceState> {
    let runner = crate::runner::runner()?;
    Some(status_line(&*runner, spec, None))
}

/// Is the unit installed, right now? The cheap half of [`probe`]: on
/// every platform that is a file check, not an OS call.
pub fn installed(spec: &ServiceSpec) -> bool {
    crate::runner::runner().is_some_and(|r| r.installed(spec))
}

/// Install: write the launch-environment file (0600, the source of truth
/// every adapter renders from) and the spec file, render the adapter's
/// artifacts, register them with the OS. Idempotent — re-running
/// overwrites this service's own artifact.
pub fn install(spec: &ServiceSpec, env: &[(String, String)]) -> Result<ServiceState, String> {
    let runner = crate::runner::runner().ok_or("no OS service runner on this machine")?;
    let _gate = lifecycle_gate();
    let mut exec = crate::io::exec;
    install_with(&*runner, spec, env, &mut exec)?;
    Ok(status_line(&*runner, spec, None))
}

/// Uninstall: stop, unregister, and take every artifact with the
/// launch-environment and spec files. The file cleanup runs even when
/// the OS refuses to unregister — a secret never outlives its
/// uninstall — and the refusal is what the caller hears.
pub fn uninstall(spec: &ServiceSpec) -> Result<ServiceState, String> {
    let runner = crate::runner::runner().ok_or("no OS service runner on this machine")?;
    let _gate = lifecycle_gate();
    let mut exec = crate::io::exec;
    uninstall_with(&*runner, spec, &mut exec)?;
    Ok(status_line(&*runner, spec, Some("uninstalled".into())))
}

/// Start or stop the installed service — the caller's manual control over
/// what the OS runner keeps. Routed through the adapter, never through a
/// shadow child, so what the caller says and what the OS runs are one
/// fact.
pub fn set_running(spec: &ServiceSpec, on: bool) -> Result<ServiceState, String> {
    let runner = crate::runner::runner().ok_or("no OS service runner on this machine")?;
    let _gate = lifecycle_gate();
    let mut exec = crate::io::exec;
    let detail = set_running_with(&*runner, spec, on, &mut exec)?;
    Ok(status_line(&*runner, spec, detail))
}

/// The launch environment changed (a rotated bearer, an added secret):
/// re-render the files and — only when the service is actually up and the
/// caller allows the bounce — restart it, so a change lands within one
/// restart exactly as replacing a supervised child would. A service the
/// user left stopped must not start itself because a value changed; its
/// files are refreshed either way, so its next start takes the new state.
///
/// One pass at a time: the installed check and the write sit in the same
/// gate, so a deferred sync can never recreate files an uninstall just
/// took away (see [`lifecycle_gate`]).
pub fn sync_launch_env(
    spec: &ServiceSpec,
    env: &[(String, String)],
    restart: bool,
) -> Result<(), String> {
    let Some(runner) = crate::runner::runner() else {
        return Ok(());
    };
    let _gate = lifecycle_gate();
    if !runner.installed(spec) {
        return Ok(()); // silent no-op: nothing is installed to sync
    }
    let mut exec = crate::io::exec;
    let up = restart && runner.running(spec);
    sync_with(&*runner, spec, env, up, &mut exec)
}

/// The same pass with the runner and executor injected: the gate, the
/// installed check, and the bounce sit behind this seam so the
/// uninstall/sync ordering is testable without an OS.
#[cfg(test)]
pub(crate) fn sync_checked_with(
    runner: &dyn OsServiceRunner,
    spec: &ServiceSpec,
    env: &[(String, String)],
    restart: bool,
    run: &mut Run<'_>,
) -> Result<(), String> {
    let _gate = lifecycle_gate();
    if !runner.installed(spec) {
        return Ok(());
    }
    let up = restart && runner.running(spec);
    sync_with(runner, spec, env, up, run)
}

// --- The injectable cores (test law without an OS to mutate) -------------------

pub(crate) fn install_with(
    runner: &dyn OsServiceRunner,
    spec: &ServiceSpec,
    env: &[(String, String)],
    run: &mut Run<'_>,
) -> Result<(), String> {
    write_secret_file(&spec.env_file, &env_to_text(env))?;
    write_spec(spec)?;
    for file in runner.install_files(spec, env) {
        write_file(&file)?;
    }
    run_all(&runner.install_cmds(spec), run)?;
    Ok(())
}

pub(crate) fn uninstall_with(
    runner: &dyn OsServiceRunner,
    spec: &ServiceSpec,
    run: &mut Run<'_>,
) -> Result<(), String> {
    // The cleanup law: the launch environment is erased even when the OS
    // refuses to unregister (a dead manager, a task locked by the
    // scheduler) — a secret never outlives its uninstall. A service the
    // OS never registered answers its own "not found" to the lenient
    // first unregister step — the clean slate it asked for; a genuine
    // refusal is kept and reported after the cleanup.
    let cmd_err = run_all(&runner.uninstall_cmds(spec), run).err();
    let mut file_err: Option<String> = None;
    for path in runner.uninstall_files(spec) {
        if let Err(e) = remove_quiet(&path) {
            file_err.get_or_insert(e);
        }
    }
    for path in [spec.env_file.clone(), spec_file(spec)] {
        if let Err(e) = remove_quiet(&path) {
            file_err.get_or_insert(e);
        }
    }
    // Prefer the file error: a leftover secret is the worse half of a
    // half-finished uninstall.
    file_err.or(cmd_err).map(Err).unwrap_or(Ok(()))
}

pub(crate) fn set_running_with(
    runner: &dyn OsServiceRunner,
    spec: &ServiceSpec,
    on: bool,
    run: &mut Run<'_>,
) -> Result<Option<String>, String> {
    let cmds = if on {
        runner.start_cmds(spec)
    } else {
        runner.stop_cmds(spec)
    };
    run_all(&cmds, run)
}

pub(crate) fn sync_with(
    runner: &dyn OsServiceRunner,
    spec: &ServiceSpec,
    env: &[(String, String)],
    restart: bool,
    run: &mut Run<'_>,
) -> Result<(), String> {
    write_secret_file(&spec.env_file, &env_to_text(env))?;
    for file in runner.install_files(spec, env) {
        write_file(&file)?;
    }
    if restart {
        run_all(&runner.restart_cmds(spec), run)?;
    }
    Ok(())
}

fn status_line(
    runner: &dyn OsServiceRunner,
    spec: &ServiceSpec,
    detail: Option<String>,
) -> ServiceState {
    let unit = match runner.name() {
        "systemd-user" => spec.id.systemd.clone(),
        "launchd" => spec.id.launchd.clone(),
        _ => spec.id.windows.clone(),
    };
    let is_up = runner.installed(spec) && runner.running(spec);
    ServiceState {
        kind: runner.name().to_string(),
        unit,
        installed: runner.installed(spec),
        running: is_up,
        detail,
    }
}

/// The one-pass-at-a-time law: without the gate a deferred sync could
/// recreate files an uninstall had just taken away, and a concurrent
/// install could race a bounce. Every mutating lifecycle verb takes it.
fn lifecycle_gate() -> parking_lot::MutexGuard<'static, ()> {
    static GATE: parking_lot::Mutex<()> = parking_lot::const_mutex(());
    GATE.lock()
}

// --- The spec file on disk ------------------------------------------------------

/// The spec file's name beside the env file: a re-render needs the full
/// spec (binary, args, env drop list) without parsing the OS's own unit
/// syntax back out.
pub(crate) fn spec_file(spec: &ServiceSpec) -> std::path::PathBuf {
    spec.data_dir.join("hearthr-service.json")
}

/// Record what the install ran with, so a later process (a daemon
/// instance, a repair pass) can re-render the artifacts without the
/// caller holding the original spec.
pub(crate) fn write_spec(spec: &ServiceSpec) -> Result<(), String> {
    let dir = spec.data_dir.as_path();
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(spec).map_err(|e| format!("serializing spec: {e}"))?;
    std::fs::write(spec_file(spec), json + "\n")
        .map_err(|e| format!("writing {}: {e}", spec_file(spec).display()))
}

/// The spec of the live install, when the artifacts are on disk. Reads
/// the JSON the install wrote — never the unit file, which is the OS's
/// format to parse.
pub fn read_spec(data_dir: &Path) -> Option<ServiceSpec> {
    let text = std::fs::read_to_string(data_dir.join("hearthr-service.json")).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::UnitId;
    use parking_lot::Mutex;
    use std::path::PathBuf;

    pub(crate) fn spec(home: &Path) -> ServiceSpec {
        ServiceSpec {
            id: UnitId {
                systemd: "mydoor.service".into(),
                launchd: "com.example.mydoor".into(),
                windows: "My Door".into(),
            },
            display_name: "My door".into(),
            binary: home.join("bin/mydoor"),
            args: vec!["--http".into(), "9".into()],
            unix_args: vec![],
            windows_args: vec![],
            windows_env_drop: vec![],
            env_file: home.join("data/launch.env"),
            data_dir: home.join("data"),
        }
    }

    struct Recorder {
        cmds: Mutex<Vec<String>>,
        fail_for: Option<String>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                cmds: Mutex::new(Vec::new()),
                fail_for: None,
            }
        }
        fn failing(verb: &str) -> Self {
            Self {
                cmds: Mutex::new(Vec::new()),
                fail_for: Some(verb.into()),
            }
        }
        fn run<'a>(&'a self) -> impl FnMut(&Command) -> Result<String, String> + 'a {
            move |cmd| {
                self.cmds.lock().push(cmd.display());
                match &self.fail_for {
                    Some(verb) if cmd.args.iter().any(|a| a == verb) => {
                        Err(format!("{verb} refused"))
                    }
                    _ => Ok("fine".into()),
                }
            }
        }
        fn seen(&self) -> Vec<String> {
            self.cmds.lock().clone()
        }
    }

    struct FakeRunner;
    impl OsServiceRunner for FakeRunner {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn installed(&self, spec: &ServiceSpec) -> bool {
            spec.data_dir.join("fake.unit").is_file()
        }
        fn install_files(
            &self,
            spec: &ServiceSpec,
            _env: &[(String, String)],
        ) -> Vec<crate::spec::ServiceFile> {
            vec![crate::spec::ServiceFile {
                path: spec.data_dir.join("fake.unit"),
                contents: "[unit]".into(),
                secret: false,
            }]
        }
        fn install_cmds(&self, _spec: &ServiceSpec) -> Vec<Command> {
            vec![Command::new("fakectl", vec!["enable".into()])]
        }
        fn uninstall_cmds(&self, _spec: &ServiceSpec) -> Vec<Command> {
            vec![Command::lenient("fakectl", vec!["disable".into()])]
        }
        fn uninstall_files(&self, spec: &ServiceSpec) -> Vec<PathBuf> {
            vec![spec.data_dir.join("fake.unit")]
        }
        fn start_cmds(&self, _spec: &ServiceSpec) -> Vec<Command> {
            vec![Command::new("fakectl", vec!["start".into()])]
        }
        fn stop_cmds(&self, _spec: &ServiceSpec) -> Vec<Command> {
            vec![Command::new("fakectl", vec!["stop".into()])]
        }
        fn restart_cmds(&self, _spec: &ServiceSpec) -> Vec<Command> {
            vec![Command::new("fakectl", vec!["restart".into()])]
        }
        fn running(&self, _spec: &ServiceSpec) -> bool {
            false
        }
    }

    /// The same fake, unregistering strictly: the shape systemd's
    /// `disable --now` and schtasks' `/Delete /F` have, where a refusal
    /// is a real error and not the clean slate a lenient step answers.
    struct StrictUninstallRunner;
    impl OsServiceRunner for StrictUninstallRunner {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn installed(&self, spec: &ServiceSpec) -> bool {
            FakeRunner.installed(spec)
        }
        fn install_files(
            &self,
            spec: &ServiceSpec,
            env: &[(String, String)],
        ) -> Vec<crate::spec::ServiceFile> {
            FakeRunner.install_files(spec, env)
        }
        fn install_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
            FakeRunner.install_cmds(spec)
        }
        fn uninstall_cmds(&self, _spec: &ServiceSpec) -> Vec<Command> {
            vec![Command::new("fakectl", vec!["disable".into()])]
        }
        fn uninstall_files(&self, spec: &ServiceSpec) -> Vec<PathBuf> {
            FakeRunner.uninstall_files(spec)
        }
        fn start_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
            FakeRunner.start_cmds(spec)
        }
        fn stop_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
            FakeRunner.stop_cmds(spec)
        }
        fn restart_cmds(&self, spec: &ServiceSpec) -> Vec<Command> {
            FakeRunner.restart_cmds(spec)
        }
        fn running(&self, spec: &ServiceSpec) -> bool {
            FakeRunner.running(spec)
        }
    }

    #[test]
    fn install_writes_three_files_and_asks_the_os_once() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        install_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "s3cr3t".into())],
            &mut rec.run(),
        )
        .unwrap();
        // env file: 0600 grammar with the pair, quoted.
        let env_text = std::fs::read_to_string(&spec.env_file).unwrap();
        assert!(env_text.contains("TOKEN=\"s3cr3t\""), "{env_text}");
        // spec file: the round-trip record.
        let back = read_spec(&spec.data_dir).unwrap();
        assert_eq!(back, spec);
        // unit artifact.
        assert!(spec.data_dir.join("fake.unit").is_file());
        assert_eq!(rec.seen(), vec!["fakectl enable"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_launch_env_file_is_owner_only_after_install() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        install_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "x".into())],
            &mut rec.run(),
        )
        .unwrap();
        let mode = std::fs::metadata(&spec.env_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "secrecy is enforced, not documented");
    }

    #[test]
    fn a_refused_enable_fails_the_install_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::failing("enable");
        let err = install_with(&FakeRunner, &spec, &[], &mut rec.run()).unwrap_err();
        assert!(err.contains("fakectl enable"), "{err}");
    }

    #[test]
    fn uninstall_takes_every_trace_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        install_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "x".into())],
            &mut rec.run(),
        )
        .unwrap();
        uninstall_with(&FakeRunner, &spec, &mut rec.run()).unwrap();
        assert!(!spec.data_dir.join("fake.unit").exists());
        assert!(!spec.env_file.exists());
        assert!(!spec_file(&spec).exists());
        let seen = rec.seen();
        assert!(seen.contains(&"fakectl disable".to_string()), "{seen:?}");
    }

    #[test]
    fn uninstall_of_nothing_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        // Never installed: the lenient disable runs, every remove is
        // NotFound-tolerant, and the answer is Ok.
        uninstall_with(&FakeRunner, &spec, &mut rec.run()).unwrap();
        assert_eq!(rec.seen(), vec!["fakectl disable"]);
    }

    #[test]
    fn a_refused_unregister_still_takes_the_secret_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        install_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "s3cr3t".into())],
            &mut rec.run(),
        )
        .unwrap();
        let angry = Recorder::failing("disable");
        let err = uninstall_with(&StrictUninstallRunner, &spec, &mut angry.run()).unwrap_err();
        // The refusal is heard…
        assert!(err.contains("fakectl disable"), "{err}");
        // …but the launch environment is gone all the same: a secret
        // never outlives its uninstall.
        assert!(
            !spec.env_file.exists(),
            "secret survived a refused uninstall"
        );
        assert!(!spec_file(&spec).exists());
        assert!(!spec.data_dir.join("fake.unit").exists());
    }

    #[test]
    fn a_sync_after_uninstall_recreates_nothing() {
        // The vulnerable sequence: a sync that checked installed-ness
        // before an uninstall finished used to recreate both files after
        // the gate had been released. The check now sits inside the gate
        // (sync_checked_with), so the post-uninstall pass is a no-op.
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        install_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "x".into())],
            &mut rec.run(),
        )
        .unwrap();
        uninstall_with(&FakeRunner, &spec, &mut rec.run()).unwrap();
        sync_checked_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "back".into())],
            true,
            &mut rec.run(),
        )
        .unwrap();
        assert!(!spec.env_file.exists(), "sync resurrected the secret");
        assert!(
            !spec.data_dir.join("fake.unit").exists(),
            "sync resurrected the unit"
        );
        assert!(
            !rec.seen().iter().any(|c| c.contains("restart")),
            "sync bounced a gone service"
        );
    }

    #[test]
    fn sync_renders_files_and_only_bounces_when_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        install_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "old".into())],
            &mut rec.run(),
        )
        .unwrap();

        // restart=false: files land, the service stays exactly as it is.
        sync_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "new".into())],
            false,
            &mut rec.run(),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&spec.env_file)
            .unwrap()
            .contains("TOKEN=\"new\""));
        assert!(!rec.seen().iter().any(|c| c.contains("restart")));

        // restart=true: one bounce carries the new environment.
        sync_with(
            &FakeRunner,
            &spec,
            &[("TOKEN".into(), "newer".into())],
            true,
            &mut rec.run(),
        )
        .unwrap();
        assert!(rec.seen().iter().any(|c| c.contains("restart")));
    }

    #[test]
    fn set_running_routes_through_the_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path());
        let rec = Recorder::new();
        set_running_with(&FakeRunner, &spec, true, &mut rec.run()).unwrap();
        set_running_with(&FakeRunner, &spec, false, &mut rec.run()).unwrap();
        assert_eq!(
            &rec.seen()[..],
            &["fakectl start".to_string(), "fakectl stop".to_string()][..]
        );
    }
}
