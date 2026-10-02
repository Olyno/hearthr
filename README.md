# hearthr

Keep the ember alive: register a named background process with the
operating system's own service manager — a **systemd user unit** on
Linux, a **launchd LaunchAgent** on macOS, a **Task Scheduler task** on
Windows — and drive it through the platform's native control tool.

One `ServiceSpec` goes in; the OS owns the process from there: at login,
across crashes, after the installing application is gone.

```rust
use hearthr::{install, ServiceSpec, UnitId};

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

// The pairs are the service's launch environment, from wherever the
// caller keeps secrets.
let env = vec![("MYDOOR_TOKEN".to_string(), "s3cr3t".to_string())];
let state = install(&spec, &env)?; // probes back: kind, unit, installed, running
```

Lifecycle: `install`, `uninstall`, `set_running`, `sync_launch_env` (re-render
the environment and bounce only if up and allowed), `probe`, `installed`,
`detect` (adapter for this machine, `None` on machines without a runner —
the caller keeps supervising its child, the default posture).

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

`cargo test` runs 22 unit tests (unit/plist/wrapper content grammar,
quoting survival, lifecycle ordering through an injected recorder,
detection branches per named OS, file modes on unix). CI gates
`fmt --check`, `clippy -D warnings`, and `test` on ubuntu + macOS +
windows.

## Provenance

Extracted and generalized from the service installer of a production
desktop application. The product couplings that lived beside these
rules — the caller's secret storage, binary publishing, and
product-specific launch flags — are policy, and stayed with that
application.

## License

Apache-2.0.
