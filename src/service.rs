//! The high road: name the service once, get a working daemon.
//!
//! [`crate::ServiceSpec`] is the honest, explicit contract — three OS
//! names, two paths, every argument spelled out — and it remains the
//! canonical form the adapters consume. This module is the ergonomic
//! roof over it: [`ServiceBuilder`] takes **one** name and derives the
//! three platform identifiers from it (`.systemd_unit`, `.job_label`
//! and `.task_name` override any one of them when a product already
//! answers to a fixed name), picks platform-conventional homes for the
//! data and environment files, and hands back a [`Service`] handle
//! whose verbs are the whole lifecycle: `install`, `start`, `stop`,
//! `restart`, `sync_env`, `status`, `wait_for_running`, `uninstall`.
//!
//! Nothing here adds policy. The launch environment still arrives as
//! pairs at the call site — the builder never stores a secret — and
//! every verb forwards to the same orchestration functions the explicit
//! API uses.
//!
//! ```no_run
//! use hearthr::Service;
//!
//! // One name, one program: the three OS identifiers are derived, the
//! // data dir lands in the platform's conventional home.
//! let (door, state) = Service::define("mydoor")
//!     .program("/home/me/.local/bin/mydoor")
//!     .args(["--port", "8765"])
//!     .install(vec![("MYDOOR_TOKEN".to_string(), "hexhex".to_string())])
//!     .expect("a runner exists on every desktop OS");
//! assert!(state.installed);
//! door.wait_for_running(std::time::Duration::from_secs(10));
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::orchestration;
use crate::spec::{ServiceSpec, ServiceState, UnitId};

/// Anything the lifecycle verbs will accept as a launch environment.
/// `&[("KEY", "value")]` and `vec![(k.to_string(), v.to_string())]`
/// both convert without a visible collection dance.
pub trait IntoEnvPairs {
    fn into_env_pairs(self) -> Vec<(String, String)>;
}

impl<K: Into<String>, V: Into<String>> IntoEnvPairs for Vec<(K, V)> {
    fn into_env_pairs(self) -> Vec<(String, String)> {
        self.into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect()
    }
}

impl<K: Clone + Into<String>, V: Clone + Into<String>> IntoEnvPairs for &[(K, V)] {
    fn into_env_pairs(self) -> Vec<(String, String)> {
        self.iter()
            .map(|(k, v)| (k.clone().into(), v.clone().into()))
            .collect()
    }
}

/// A service under construction. Every setter consumes `self`, so the
/// whole picture reads as one sentence; [`build`](Self::build) is the
/// only step that can fail (and it touches the disk for nothing beyond
/// creating the data dir).
#[derive(Clone, Debug, Default)]
pub struct ServiceBuilder {
    name: String,
    display: Option<String>,
    systemd_unit: Option<String>,
    job_label: Option<String>,
    task_name: Option<String>,
    reverse_domain: Option<String>,
    program: Option<PathBuf>,
    args: Vec<String>,
    unix_args: Vec<String>,
    windows_args: Vec<String>,
    windows_env_drop: Vec<String>,
    data_dir: Option<PathBuf>,
    env_file: Option<PathBuf>,
}

impl ServiceBuilder {
    /// Start from one name. It is a human label ("My Door", "mydoor",
    /// "my-door" all work): the platform grammars are derived from it
    /// (see [`Service::derive_unit_id`]), and it is what a person will
    /// read in `systemctl --user list-units` output beside the
    /// description line.
    pub fn new(name: impl Into<String>) -> ServiceBuilder {
        ServiceBuilder {
            name: name.into(),
            ..ServiceBuilder::default()
        }
    }

    /// The human-readable description carried into every artifact.
    /// Defaults to the name as typed.
    pub fn display(mut self, text: impl Into<String>) -> ServiceBuilder {
        self.display = Some(text.into());
        self
    }

    /// Override the systemd unit file name entirely; `.service` is
    /// appended when absent. Use when the product already ships with a
    /// published unit name — a service registered under one name must
    /// not answer under another.
    pub fn systemd_unit(mut self, unit: impl Into<String>) -> ServiceBuilder {
        self.systemd_unit = Some(unit.into());
        self
    }

    /// Override the launchd job label (reverse-DNS form).
    pub fn job_label(mut self, label: impl Into<String>) -> ServiceBuilder {
        self.job_label = Some(label.into());
        self
    }

    /// Override the Task Scheduler task name.
    pub fn task_name(mut self, task: impl Into<String>) -> ServiceBuilder {
        self.task_name = Some(task.into());
        self
    }

    /// The DNS prefix the derived launchd label answers under, when
    /// `.job_label` is not set. Defaults to `com.hearthr`.
    pub fn reverse_domain(mut self, domain: impl Into<String>) -> ServiceBuilder {
        self.reverse_domain = Some(domain.into());
        self
    }

    /// The binary the service runs. Must be an absolute path — a
    /// relative one resolves against whatever cwd the OS manager
    /// happens to use, which is nobody's app directory.
    pub fn program(mut self, path: impl Into<PathBuf>) -> ServiceBuilder {
        self.program = Some(path.into());
        self
    }

    /// The service's command line, minus any credential.
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> ServiceBuilder {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Extra argv appended only on the unix units.
    pub fn unix_args(
        mut self,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> ServiceBuilder {
        self.unix_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Extra argv appended only on the Windows wrapper.
    pub fn windows_args(
        mut self,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> ServiceBuilder {
        self.windows_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Environment names that must not reach the Windows task
    /// environment (see [`ServiceSpec::windows_env_drop`]).
    pub fn windows_env_drop(
        mut self,
        keys: impl IntoIterator<Item = impl Into<String>>,
    ) -> ServiceBuilder {
        self.windows_env_drop = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Where the spec file, the Windows artifacts, and the stderr log
    /// live. Defaults to the platform's own per-service directory
    /// (XDG data dir / Application Support / %APPDATA%) under the slug.
    pub fn data_dir(mut self, path: impl Into<PathBuf>) -> ServiceBuilder {
        self.data_dir = Some(path.into());
        self
    }

    /// The 0600 launch-environment file. Defaults to `launch.env`
    /// inside [`data_dir`](Self::data_dir).
    pub fn env_file(mut self, path: impl Into<PathBuf>) -> ServiceBuilder {
        self.env_file = Some(path.into());
        self
    }

    /// Validate, derive, and materialise the data directory. The
    /// secret-free [`Service`] that comes back can be cloned, stored,
    /// or serialised through its [`spec`](Service::spec).
    pub fn build(self) -> Result<Service, String> {
        let words = split_words(&self.name);
        if words.is_empty() {
            return Err("service name is empty".into());
        }
        let program = self
            .program
            .clone()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("program path is required")?;
        if !program.is_absolute() {
            return Err(format!("program path must be absolute (the OS manager's cwd is not the app directory): {program:?}"));
        }
        let slug = words.join("-").to_lowercase();
        let data_dir = self
            .data_dir
            .clone()
            .unwrap_or_else(|| default_data_dir(&slug));
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("cannot create data dir {data_dir:?}: {e}"))?;
        let env_file = self
            .env_file
            .clone()
            .unwrap_or_else(|| data_dir.join("launch.env"));
        let id = UnitId {
            systemd: self.systemd_unit.clone().map_or_else(
                || format!("{slug}.service"),
                |unit| {
                    if unit.ends_with(".service") {
                        unit
                    } else {
                        format!("{unit}.service")
                    }
                },
            ),
            launchd: self.job_label.clone().unwrap_or_else(|| {
                format!(
                    "{}.{slug}",
                    self.reverse_domain
                        .clone()
                        .unwrap_or_else(|| "com.hearthr".into())
                )
            }),
            windows: self.task_name.clone().unwrap_or_else(|| title_case(&words)),
        };
        Ok(Service {
            spec: ServiceSpec {
                id,
                display_name: self.display.clone().unwrap_or_else(|| {
                    let text = self.name.trim();
                    if text.is_empty() {
                        title_case(&words)
                    } else {
                        text.to_string()
                    }
                }),
                binary: program,
                args: self.args.clone(),
                unix_args: self.unix_args.clone(),
                windows_args: self.windows_args.clone(),
                windows_env_drop: self.windows_env_drop.clone(),
                env_file,
                data_dir,
            },
        })
    }

    /// `build()` then `install(env)` — the one-call version.
    pub fn install(self, env: impl IntoEnvPairs) -> Result<(Service, ServiceState), String> {
        let service = self.build()?;
        let state = service.install(env)?;
        Ok((service, state))
    }
}

/// A built service: the spec plus the lifecycle verbs. Cheap to clone;
/// it holds no secret and touches nothing until a verb is called.
#[derive(Clone, Debug)]
pub struct Service {
    spec: ServiceSpec,
}

impl Service {
    /// One name in, the whole sentence:
    /// `Service::define("mydoor").program("/usr/bin/mydoor")…` —
    /// see [`ServiceBuilder`] for every setter.
    pub fn define(name: impl Into<String>) -> ServiceBuilder {
        ServiceBuilder::new(name)
    }

    /// Rehydrate a handle from the spec file a previous install left in
    /// `data_dir`. Lets a second command of the same app drive the
    /// service the first one installed without carrying the spec
    /// through its own arguments.
    pub fn load(data_dir: impl Into<PathBuf>) -> Result<Service, String> {
        let data_dir = data_dir.into();
        orchestration::read_spec(&data_dir)
            .map(|spec| Service { spec })
            .ok_or_else(|| format!("no hearthr spec file in {data_dir:?}"))
    }

    /// The canonical explicit spec this handle drives — pass it to any
    /// of the low-level functions in the crate root.
    pub fn spec(&self) -> &ServiceSpec {
        &self.spec
    }

    /// Derive the three platform names from one human label, exactly as
    /// [`ServiceBuilder::build`] does. Useful when a caller wants the
    /// low-level [`ServiceSpec`] spelled out but with conventional names.
    pub fn derive_unit_id(name: impl AsRef<str>, reverse_domain: impl AsRef<str>) -> UnitId {
        let words = split_words(name.as_ref());
        let slug = if words.is_empty() {
            "service".into()
        } else {
            words.join("-").to_lowercase()
        };
        UnitId {
            systemd: format!("{slug}.service"),
            launchd: format!("{}.{}", reverse_domain.as_ref(), slug),
            windows: title_case(&words),
        }
    }

    /// Write the artifacts and register with the OS. Idempotent: a
    /// second install overwrites this service's own unit and keeps
    /// everything else standing.
    pub fn install(&self, env: impl IntoEnvPairs) -> Result<ServiceState, String> {
        orchestration::install(&self.spec, &env.into_env_pairs())
    }

    /// Unregister and delete every artifact, including every trace of
    /// the launch environment.
    pub fn uninstall(&self) -> Result<ServiceState, String> {
        orchestration::uninstall(&self.spec)
    }

    pub fn start(&self) -> Result<ServiceState, String> {
        orchestration::set_running(&self.spec, true)
    }

    pub fn stop(&self) -> Result<ServiceState, String> {
        orchestration::set_running(&self.spec, false)
    }

    /// Stop then start. A stop of a unit the OS believes is not running
    /// is tolerated; the start result is what the caller hears.
    pub fn restart(&self) -> Result<ServiceState, String> {
        self.stop().ok();
        self.start()
    }

    /// Re-render the launch-environment files with a fresh pair list
    /// and, when `restart`, bounce the service onto them.
    pub fn sync_env(&self, env: impl IntoEnvPairs, restart: bool) -> Result<(), String> {
        orchestration::sync_launch_env(&self.spec, &env.into_env_pairs(), restart)
    }

    /// The artifact is registered on this machine.
    pub fn is_installed(&self) -> bool {
        orchestration::installed(&self.spec)
    }

    /// The OS's own answer about this service. `None` when the machine
    /// has no adapter at all (the caller keeps supervising its child).
    pub fn status(&self) -> Option<ServiceState> {
        orchestration::probe(&self.spec)
    }

    /// Poll [`status`](Self::status) until the OS says the unit is
    /// running, or the deadline passes. Service managers start units
    /// asynchronously — a UI that flips a toggle wants to know when the
    /// green dot is real, not when the command returned.
    pub fn wait_for_running(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.status().is_some_and(|state| state.running) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Split a human label into words: anything that is not alphanumeric is
/// a separator (space, dash, underscore, dot, slash — all of them).
pub(crate) fn split_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Title-case the derived task-name/display label: "my door", "mydoor"
/// and "my-door" all become "My Door" / "Mydoor" — each word's first
/// character upper, the rest left alone so "MyApp" stays "MyApp".
pub(crate) fn title_case(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The platform's conventional per-service home, under the slug.
fn default_data_dir(slug: &str) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|home| PathBuf::from(home).join("AppData\\Roaming"))
            })
            .unwrap_or_else(|| PathBuf::from(slug));
        return root.join(slug);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library/Application Support")
                .join(slug);
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join(slug);
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/share").join(slug);
        }
    }
    PathBuf::from(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> ServiceBuilder {
        ServiceBuilder::new("My Door").program("/usr/bin/mydoor")
    }

    #[test]
    fn one_name_derives_the_three_platform_grammars() {
        let service = builder().build().unwrap();
        assert_eq!(service.spec().id.systemd, "my-door.service");
        assert_eq!(service.spec().id.launchd, "com.hearthr.my-door");
        assert_eq!(service.spec().id.windows, "My Door");
        assert_eq!(service.spec().display_name, "My Door");
    }

    #[test]
    fn a_single_word_slug_keeps_its_shape() {
        let service = ServiceBuilder::new("mydoor")
            .program("/usr/local/bin/mydoor")
            .build()
            .unwrap();
        assert_eq!(service.spec().id.systemd, "mydoor.service");
        assert_eq!(service.spec().id.launchd, "com.hearthr.mydoor");
        assert_eq!(service.spec().id.windows, "Mydoor");
    }

    #[test]
    fn every_derivation_is_overridable() {
        let service = builder()
            .systemd_unit("my-endpoint")
            .job_label("com.example.endpoint")
            .task_name("My Endpoint")
            .build()
            .unwrap();
        assert_eq!(service.spec().id.systemd, "my-endpoint.service");
        assert_eq!(service.spec().id.launchd, "com.example.endpoint");
        assert_eq!(service.spec().id.windows, "My Endpoint");
    }

    #[test]
    fn reverse_domain_swaps_the_label_prefix() {
        let service = builder().reverse_domain("com.example").build().unwrap();
        assert_eq!(service.spec().id.launchd, "com.example.my-door");
    }

    #[test]
    fn punctuation_and_mixed_separators_split_into_words() {
        assert_eq!(split_words("my_door.app/x"), ["my", "door", "app", "x"]);
        assert_eq!(title_case(&split_words("MyApp")).as_str(), "MyApp");
        assert_eq!(
            Service::derive_unit_id("My Endpoint", "com.p").windows,
            "My Endpoint"
        );
    }

    #[test]
    fn build_rejects_an_impossible_service() {
        assert!(ServiceBuilder::new("  ")
            .program("/usr/bin/x")
            .build()
            .is_err());
        assert!(ServiceBuilder::new("door").build().is_err());
        assert!(ServiceBuilder::new("door")
            .program("target/debug/door")
            .build()
            .is_err());
    }

    #[test]
    fn paths_default_to_the_platform_home_and_can_be_moved() {
        let dir = tempfile::tempdir().unwrap();
        let service = builder().data_dir(dir.path().join("svc")).build().unwrap();
        assert!(dir.path().join("svc").is_dir());
        assert_eq!(
            service.spec().env_file,
            dir.path().join("svc").join("launch.env")
        );
        // An explicit env_file survives the data dir defaulting.
        let service = builder()
            .data_dir(dir.path().join("svc"))
            .env_file(dir.path().join("custom.env"))
            .build()
            .unwrap();
        assert_eq!(service.spec().env_file, dir.path().join("custom.env"));
    }

    #[test]
    fn handle_rehydrates_from_the_spec_file_install_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let service = builder().data_dir(dir.path()).build().unwrap();
        // install() writes this file through the core; write the same
        // bytes to prove the round-trip without touching the OS.
        std::fs::write(
            dir.path().join("hearthr-service.json"),
            serde_json::to_vec(service.spec()).unwrap(),
        )
        .unwrap();
        let loaded = Service::load(dir.path()).unwrap();
        assert_eq!(loaded.spec(), service.spec());
        assert!(Service::load(dir.path().join("nowhere")).is_err());
    }

    #[test]
    fn env_pairs_convert_from_both_spellings() {
        let owned: Vec<(String, String)> = vec![("K".into(), "v".into())];
        assert_eq!(owned.into_env_pairs().len(), 1);
        let borrowed = [("TOKEN", "s3cr3t"), ("PORT", "8765")];
        assert_eq!(IntoEnvPairs::into_env_pairs(&borrowed[..]).len(), 2);
    }
}
