//! hearthr keeps a process registered with the operating system's own
//! background runner: a systemd **user** unit on Linux, a launchd
//! LaunchAgent on macOS, a Task Scheduler task on Windows. One [`ServiceSpec`]
//! goes in; the OS owns the process from there — at login, across crashes,
//! after the installing app is gone.
//!
//! Start at [`Service`] / [`ServiceBuilder`] for the ergonomic road:
//! one name in, the three platform identifiers derived, lifecycle
//! verbs on a handle. [`ServiceSpec`] remains the canonical explicit
//! contract every adapter consumes, and the free functions
//! ([`install`], [`uninstall`], [`set_running`], [`sync_launch_env`],
//! [`probe`]) are its direct verbs — the high-level handle forwards to
//! exactly those.
//!
//! The crate is deliberately **mechanism, not policy**:
//!
//! - It generates the artifact each OS demands and drives the platform's
//!   own control commands (`systemctl --user`, `launchctl`, `schtasks`).
//!   Artifact generation is pure — every adapter is fully testable on any
//!   machine, and nothing touches the OS until a lifecycle call asks it to.
//! - It never runs a shell: every command is program + argv ([`Command`]),
//!   the injection floor the whole design keeps.
//! - It carries no secret storage. The launch environment arrives as a
//!   pair list; the crate writes the files the OS manager reads (0600 for
//!   any artifact holding a value, unit files world-readable), and where
//!   a value's characters cannot survive a platform's quoting grammar the
//!   spec names it for the Windows filter ([`ServiceSpec::windows_env_drop`]).
//!   Deciding *which* secret and *where* it lives (OS keyring, vault,
//!   nothing) is the caller's law.
//! - It picks the adapter for the machine it runs on via [`detect`], and
//!   answers `None` when no runner exists — the caller keeps supervising
//!   its child process, which is every machine's default posture.
//!
//! ```no_run
//! use hearthr::{detect, install, uninstall, ServiceSpec, UnitId};
//! use std::path::PathBuf;
//!
//! let spec = ServiceSpec {
//!     id: UnitId {
//!         systemd: "mydoor.service".into(),
//!         launchd: "com.example.mydoor".into(),
//!         windows: "My Door".into(),
//!     },
//!     display_name: "My door".into(),
//!     binary: PathBuf::from("/home/me/.local/bin/mydoor"),
//!     args: vec!["--port".into(), "8765".into()],
//!     unix_args: vec![],
//!     windows_args: vec![],
//!     windows_env_drop: vec![],
//!     env_file: PathBuf::from("/home/me/.local/share/mydoor/launch.env"),
//!     data_dir: PathBuf::from("/home/me/.local/share/mydoor"),
//! };
//! // The pairs are the service's launch environment, from wherever the
//! // caller keeps secrets.
//! let env = vec![("MYDOOR_TOKEN".to_string(), "hexhex".to_string())];
//! install(&spec, &env).expect("no OS runner on this machine?");
//! // ... later, when the offer is withdrawn:
//! uninstall(&spec).ok();
//! ```
//!
//! # The lifecycle, in one breath
//!
//! [`install`] writes the launch-environment file, the spec file, and the
//! adapter's artifact, then registers them with the OS. [`set_running`]
//! starts/stops the registered unit — routed through the adapter, never
//! through a shadow child, so what the caller says and what the OS runs
//! are one fact. [`sync_launch_env`] re-renders the environment files with
//! a fresh pair list and, when allowed, bounces the service — the
//! service's version of replacing a supervised child. [`uninstall`] stops,
//! unregisters, and takes every trace of the secret with the artifacts:
//! the file cleanup happens even when the OS refuses to unregister, and
//! the refusal is what the caller hears.
//!
//! Reinstall is idempotent: it overwrites the caller's own artifact and
//! keeps everything else standing. A machine without a runner gets an
//! error, never a panic.

mod io;
pub mod launchd;
mod orchestration;
mod runner;
pub mod service;
mod spec;
pub mod systemd;
pub mod windows;
pub use io::exec;
pub use orchestration::{
    install, installed, probe, read_spec, set_running, sync_launch_env, uninstall,
};
pub use runner::{detect, detect_os, OsServiceRunner};
pub use service::{IntoEnvPairs, Service, ServiceBuilder};
pub use spec::{Command, ServiceFile, ServiceSpec, ServiceState, UnitId};
