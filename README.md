# hearthr

Keep the ember alive: register a named background process with the
operating system's own service manager — a **systemd user unit** on
Linux, a **launchd LaunchAgent** on macOS, a **Task Scheduler task** on
Windows — and drive it through the platform's native control tool.

## Quickstart — one name, one sentence

```rust
use hearthr::Service;
use std::time::Duration;

// The three OS identifiers are derived from the single name; the data
// dir lands in the platform's conventional home (XDG / Application
// Support / %APPDATA%). The launch environment arrives as pairs at the
// call site — the crate never stores a secret.
let (door, state) = Service::define("mydoor")
    .program("/home/me/.local/bin/mydoor")
    .args(["--port", "8765"])
    .install(vec![("MYDOOR_TOKEN".to_string(), "s3cr3t".to_string())])?;

assert!(state.installed);
door.wait_for_running(Duration::from_secs(10)); // green dot is real
door.sync_env(vec![("MYDOOR_TOKEN".to_string(), "rotated".to_string())], true)?;
door.stop()?;
door.uninstall()?; // every trace of the secret goes with it
# Ok::<(), String>(())
```

Need exact control over what the OS registers? Every derivation is
overridable — `.systemd_unit("my-endpoint")`, `.job_label("com.example.endpoint")`,
`.task_name("My Endpoint")`, `.reverse_domain("com.example")`,
`.data_dir(...)`, `.env_file(...)`, `.windows_env_drop(["TOKEN"])`. And
`Service::spec()` hands back the plain `ServiceSpec` for the low-level
functions below.

## The low road: explicit everything

The high-level `Service` is the ergonomic roof; the explicit contract
remains canonical. A `ServiceSpec` spells all three platform names, the
program, and the paths, and the free functions drive it:

```rust
use hearthr::{install, uninstall, ServiceSpec, UnitId};

let spec = ServiceSpec {
    id: UnitId {
        systemd: "mydoor.service".into(),
        launchd: "com.example.mydoor".into(),
        windows: "My Door".into(),
    },
    display_name: "My door endpoint".into(),
    binary: "/home/me/.local/bin/mydoor".into(),
    args: vec!["--port".into(), "8765".into()],
    unix_args: vec![],          // extra flags for the unix units
    windows_args: vec![],       // extra flags for the task wrapper
    windows_env_drop: vec![],   // env names cmd quoting cannot promise
    env_file: "/home/me/.local/share/mydoor/launch.env".into(),
    data_dir: "/home/me/.local/share/mydoor".into(),
};
let env = vec![("MYDOOR_TOKEN".to_string(), "s3cr3t".to_string())];
let state = install(&spec, &env)?; // probes back: kind, unit, installed, running
```

Lifecycle verbs: `install`, `uninstall`, `set_running`,
`sync_launch_env` (re-render the environment and bounce only if up and
allowed), `probe`, `installed`, `detect` (adapter for this machine,
`None` on machines without a runner — the caller keeps supervising its
child, the default posture).

`Service::load(data_dir)` rehydrates a handle from the spec file a
previous install left behind — a second command of the same app drives
the service the first one installed without carrying the spec around.

### The spec, field by field

| field | what it is |
| --- | --- |
| `id` | the names the three OS runners know this service by: `id.systemd` (unit file name, `mydoor.service`), `id.launchd` (reverse-DNS job label, `com.example.mydoor`), `id.windows` (free-text Task Scheduler name, `My Door`). Each platform spells identifiers in its own grammar; none of them may drift — a service registered under one name must not answer under another. |
| `display_name` | the human one-liner the unit carries as its description (`Description=` in the systemd unit). |
| `binary` | the program to run, **absolute path required**: the OS manager's cwd is nobody's app directory, and the path must resolve after the installing app is gone (a published location, never a build dir). |
| `args` | the common command line: `binary + args` on every platform. |
| `unix_args` | extra argv appended **only** on systemd/launchd — flags whose meaning relies on the launch env being a file/dict the unit re-reads. Never reach the Windows wrapper. |
| `windows_args` | extra argv appended **only** in the task wrapper — Windows-only flags (a tray flag). Never reach the unix units. |
| `windows_env_drop` | env **names** excluded from the Windows wrapper only (they still land in the env file and the other adapters). cmd's `set "K=V"` grammar cannot promise some values verbatim; since 0.1 an unsafe value fails the install at preflight instead, so this list is the opt-out for names you know are unsafe or unneeded on Windows. |
| `env_file` | the 0600 `launch.env` holding the launch environment as `KEY="value"` lines. On systemd the unit references it via `EnvironmentFile=` and re-reads it at every start (that's why the unit itself stays secret-free and world-readable); on every platform it is written, because it is the caller's re-render source of truth for `sync_launch_env` and repair. Uninstall removes it. |
| `data_dir` | the per-service home the install owns: the spec file (`hearthr-service.json`, what `Service::load` rehydrates), the Windows artifacts (wrapper `.cmd`, task `.xml` — the scheduler keeps no readable copy), and the stderr log the launchd/Windows artifacts point into. Uninstall removes everything it wrote there. |

The final command line is `binary + args + unix_args` on unix and
`binary + args + windows_args` on Windows. Secrets never ride argv —
they ride `env_file`. Not spelled out in the spec: the launch
environment itself (never stored by the crate, arriving as pairs at
every install/sync), and the restart policy (fixed law per adapter:
`Restart=on-failure` + rate ceiling, `KeepAlive` +
`ThrottleInterval`, `RestartOnFailure` — not a knob).

## What this crate deliberately is not

- **Not a secret store.** The launch environment arrives as a pair list.
  hearthr writes the files the OS manager reads (0600 for any artifact
  holding a value; unit files stay world-readable and name the env file
  only by path, on systemd). Which secret lives where (OS keyring,
  vault, nowhere) is the caller's law.
- **No shell, ever.** Every OS call is program + argv (`Command`). The
  injection floor: a path with a space, an `&` inside a value, a `$` in
  a systemd unit — quoted per the platform's grammar or filtered by
  `windows_env_drop`, tested in both directions.
- **Not a supervisor.** The OS restarts what crashes
  (`Restart=on-failure` + rate ceiling, `ThrottleInterval`,
  `RestartOnFailure`). The retry ladder that answers to humans belongs to
  the caller.

## Design law, in three lines

1. Artifact **generation is pure** — every adapter testable on any
   machine; nothing touches the OS until a lifecycle call asks it.
2. The **artifact file is installed-ness** — a `stat`, not an OS query.
3. **Reinstall is idempotent**; uninstall removes every trace of the
   launch environment; a lenient first step (`bootout`, `/End`) makes a
   clean slate even where the OS never saw the job.

## Tests and CI

`cargo test` runs 42 unit tests plus 2 doc examples (builder name
derivation and overrides, unit/plist/wrapper content grammar, quoting
survival, preflight rejection of what a platform grammar cannot carry,
lifecycle ordering through an injected recorder, detection
branches per named OS, file modes on unix). CI gates `fmt --check`,
`clippy -D warnings`, and `test` on ubuntu + macOS + windows.

## Provenance

Extracted and generalized from the service installer of a production
desktop application. The product couplings that lived beside these
rules — the caller's secret storage, binary publishing, and
product-specific launch flags — are policy, and stayed with that
application.

## License

Apache-2.0.
