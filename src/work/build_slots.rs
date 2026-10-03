//! Cross-lane cargo build slots (GitHub issue #65).
//!
//! Parallel lanes each run `cargo` directly from inside their agent, and
//! cargo defaults to one compile job per core, so a dozen lanes can start a
//! dozen full builds at the same moment and run the machine out of memory.
//! tm cannot see inside an agent's shell commands, but it does control the
//! environment every lane is launched with. When `[work] build_slots` is a
//! positive N, [`crate::work::run::prepare_run_lane`] deploys a tiny `cargo`
//! shim (see [`deploy_shim`]) and puts its directory first on the lane's
//! `PATH` (see [`lane_env`]), so any `cargo` the agent runs is this wrapper
//! instead.
//!
//! The shim re-execs `tm` with [`WRAPPER_ARG`], which lands in
//! [`run_wrapper`]:
//!
//! 1. Commands that do not compile anything (`cargo fmt`, `cargo metadata`,
//!    `cargo --version`, ...; see [`needs_slot`]) skip the slot entirely, as
//!    does a cargo nested inside one that already holds a slot
//!    ([`HELD_ENV`]), which would otherwise deadlock with one slot.
//! 2. Everything else takes one of N lock files under
//!    `~/.local/share/tskmstr/build-slots/locks` with an exclusive `flock`
//!    ([`acquire_slot`]). The lock is released by the kernel when the
//!    process exits, however it exits, so a killed build never strands a
//!    slot. While all N are held it prints one "queued" line to stderr and
//!    polls until one frees up.
//! 3. It sets `CARGO_BUILD_JOBS` to cores / N (unless the caller already set
//!    it), runs the real `cargo` (the next one on `PATH` after the shim
//!    directory) with the original arguments and inherited stdio, and exits
//!    with its exit code.
//!
//! The slot pool is per-user, not per-repo: every lane on the machine shares
//! the same lock files, which is the point. A lane uses the N from its own
//! config, so two repos configured with different N share the files of the
//! smaller N.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The first argument the `cargo` shim passes to `tm` to reach
/// [`run_wrapper`]. `main` checks for it before any CLI parsing or config
/// loading, so a wrapped `cargo` costs no config read.
pub const WRAPPER_ARG: &str = "__cargo-wrap";

/// How many build slots the lane was launched with (the lane's
/// `[work] build_slots`).
pub const SLOTS_ENV: &str = "TSKMSTR_BUILD_SLOTS";

/// The directory holding the slot lock files.
pub const LOCK_DIR_ENV: &str = "TSKMSTR_BUILD_SLOT_LOCK_DIR";

/// The directory holding the `cargo` shim, skipped when looking for the
/// real `cargo` on `PATH`.
pub const SHIM_DIR_ENV: &str = "TSKMSTR_BUILD_SLOT_SHIM_DIR";

/// Set in the real cargo's environment while the wrapper holds a slot, so a
/// cargo nested inside it (a build script or test that shells out to cargo)
/// runs straight through instead of waiting on a slot its parent holds.
pub const HELD_ENV: &str = "TSKMSTR_BUILD_SLOT_HELD";

/// How long [`acquire_slot`] sleeps between polls while every slot is held.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Global cargo options that take a separate value (`--config x=y`), which
/// [`needs_slot`] must skip along with the option to find the subcommand.
const GLOBAL_OPTIONS_WITH_VALUE: &[&str] = &["--config", "--color", "-Z", "-C", "--explain"];

/// Subcommands that never compile, so never wait on a build slot.
const NON_COMPILING_SUBCOMMANDS: &[&str] = &[
    "add",
    "clean",
    "config",
    "fetch",
    "fmt",
    "generate-lockfile",
    "help",
    "info",
    "init",
    "locate-project",
    "login",
    "logout",
    "metadata",
    "new",
    "owner",
    "pkgid",
    "read-manifest",
    "remove",
    "report",
    "rm",
    "search",
    "tree",
    "uninstall",
    "update",
    "vendor",
    "verify-project",
    "version",
    "yank",
];

/// The per-user build-slot root: `<home>/.local/share/tskmstr/build-slots`.
/// The shim lives in its `bin` subdirectory and the lock files in `locks`.
pub fn root_dir(home: &Path) -> PathBuf {
    home.join(".local/share/tskmstr/build-slots")
}

/// Whether `cargo <args>` compiles anything and so must take a build slot.
///
/// Leading global options (`+toolchain`, `-v`, `--config <v>`, ...) are
/// skipped to find the subcommand. A known non-compiling subcommand, or no
/// subcommand at all (`cargo --version`, `cargo --list`, bare `cargo`),
/// skips the slot. Anything else, including unknown external subcommands
/// such as `cargo nextest`, takes one: wrongly queueing a cheap command only
/// costs a wait, while wrongly skipping a build is what runs the machine out
/// of memory.
pub fn needs_slot(args: &[OsString]) -> bool {
    let mut iter = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = iter.next() {
        if arg.starts_with('+') {
            continue;
        }
        if arg.starts_with('-') {
            if GLOBAL_OPTIONS_WITH_VALUE.contains(&arg.as_ref()) {
                iter.next();
            }
            continue;
        }
        return !NON_COMPILING_SUBCOMMANDS.contains(&arg.as_ref());
    }
    false
}

/// `CARGO_BUILD_JOBS` for one slot: the machine's cores split evenly across
/// `slots`, never below one.
pub fn jobs_per_slot(cores: usize, slots: u32) -> usize {
    let slots = (slots as usize).max(1);
    (cores / slots).max(1)
}

/// The `cargo` shim's script body: a POSIX `sh` script that re-execs
/// `tm_exe` with [`WRAPPER_ARG`] and the original arguments.
pub fn shim_script(tm_exe: &Path) -> String {
    format!(
        "#!/bin/sh\n# Generated by tm (GitHub issue #65): routes cargo through tm's build slots.\nexec {} {WRAPPER_ARG} \"$@\"\n",
        sh_quote(&tm_exe.to_string_lossy())
    )
}

/// Write the `cargo` shim (see [`shim_script`]) to `<root>/bin/cargo`,
/// executable, and create `<root>/locks`. Rewritten on every call so the
/// shim always points at the `tm` binary launching the lane. Returns the
/// shim directory, `<root>/bin`.
pub fn deploy_shim(root: &Path, tm_exe: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let shim_dir = root.join("bin");
    std::fs::create_dir_all(&shim_dir)?;
    std::fs::create_dir_all(root.join("locks"))?;
    // Written to a temp file and renamed into place, so a lane running the
    // shim at this moment never sees a half-written script.
    let shim = shim_dir.join("cargo");
    let tmp = shim_dir.join(format!(".cargo.{}.tmp", std::process::id()));
    std::fs::write(&tmp, shim_script(tm_exe))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &shim)?;
    Ok(shim_dir)
}

/// The environment a lane is launched with to route its `cargo` through the
/// wrapper: `PATH` with `shim_dir` first (any existing `shim_dir` entries,
/// from a lane launched inside another lane, removed), plus [`SLOTS_ENV`],
/// [`LOCK_DIR_ENV`], and [`SHIM_DIR_ENV`].
pub fn lane_env(
    slots: u32,
    root: &Path,
    shim_dir: &Path,
    current_path: Option<&OsStr>,
) -> Vec<(String, String)> {
    let mut entries = vec![shim_dir.to_path_buf()];
    if let Some(path) = current_path {
        entries.extend(std::env::split_paths(path).filter(|entry| entry != shim_dir));
    }
    let path = std::env::join_paths(entries)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_else(|_| shim_dir.to_string_lossy().into_owned());
    vec![
        ("PATH".to_string(), path),
        (SLOTS_ENV.to_string(), slots.to_string()),
        (
            LOCK_DIR_ENV.to_string(),
            root.join("locks").to_string_lossy().into_owned(),
        ),
        (
            SHIM_DIR_ENV.to_string(),
            shim_dir.to_string_lossy().into_owned(),
        ),
    ]
}

/// The first executable `cargo` on `path` outside `shim_dir`.
pub fn find_real_cargo(path: &OsStr, shim_dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let shim_canonical = std::fs::canonicalize(shim_dir).ok();
    std::env::split_paths(path)
        .filter(|dir| {
            dir != shim_dir
                && (shim_canonical.is_none() || std::fs::canonicalize(dir).ok() != shim_canonical)
        })
        .map(|dir| dir.join("cargo"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Take one of `slots` exclusive lock files (`slot-<i>.lock`) in
/// `lock_dir`, blocking until one is free. `on_queue` is called once, the
/// first time every slot is found held. The slot is held for as long as the
/// returned [`File`] stays open, and released when it is dropped or the
/// process exits.
pub fn acquire_slot(lock_dir: &Path, slots: u32, on_queue: &mut dyn FnMut()) -> io::Result<File> {
    let slots = slots.max(1);
    let mut queued = false;
    loop {
        for slot in 0..slots {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(lock_dir.join(format!("slot-{slot}.lock")))?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => return Err(err),
            }
        }
        if !queued {
            queued = true;
            on_queue();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The wrapper's inputs, read from the environment by [`run_wrapper`] and
/// built directly by tests.
#[derive(Debug, Clone, Default)]
pub struct WrapperEnv {
    /// [`SLOTS_ENV`], parsed. `None` or `0` runs cargo straight through.
    pub slots: Option<u32>,
    /// [`LOCK_DIR_ENV`].
    pub lock_dir: Option<PathBuf>,
    /// [`SHIM_DIR_ENV`].
    pub shim_dir: Option<PathBuf>,
    /// `PATH`, searched for the real cargo.
    pub path: Option<OsString>,
    /// Whether [`HELD_ENV`] is set (this cargo is nested inside a slot).
    pub held: bool,
    /// Whether `CARGO_BUILD_JOBS` is already set, in which case it is left
    /// alone.
    pub jobs_set: bool,
    /// Available cores, for [`jobs_per_slot`].
    pub cores: usize,
}

/// Run the real cargo with `args` under `env`'s slot rules, returning its
/// exit code (`128 + signal` when it was killed by a signal). stdin,
/// stdout, and stderr are inherited, so cargo's output passes through
/// unchanged. Errors with [`io::ErrorKind::NotFound`] when no real cargo is
/// on `PATH` outside the shim directory.
pub fn run_wrapped(args: &[OsString], env: &WrapperEnv) -> io::Result<i32> {
    use std::os::unix::process::ExitStatusExt;

    let path = env.path.clone().unwrap_or_default();
    let shim_dir = env.shim_dir.clone().unwrap_or_default();
    let real = find_real_cargo(&path, &shim_dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no real cargo found on PATH outside tm's shim directory",
        )
    })?;

    let mut command = std::process::Command::new(real);
    command.args(args);

    let slot = match (env.slots, env.lock_dir.as_deref()) {
        (Some(slots), Some(lock_dir)) if slots > 0 && !env.held && needs_slot(args) => {
            std::fs::create_dir_all(lock_dir)?;
            let slot = acquire_slot(lock_dir, slots, &mut || {
                eprintln!("tm: all {slots} build slots are in use; queued for a build slot...");
            })?;
            command.env(HELD_ENV, "1");
            if !env.jobs_set {
                command.env(
                    "CARGO_BUILD_JOBS",
                    jobs_per_slot(env.cores, slots).to_string(),
                );
            }
            Some(slot)
        }
        _ => None,
    };

    let status = command.status()?;
    drop(slot);
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

/// The cargo arguments when `argv` (a full `tm` argv, program name first)
/// is a shim invocation, `tm __cargo-wrap <args>`, and `None` otherwise.
/// `main` checks this before clap sees `argv`, so cargo's own flags
/// (`--help`, `-V`) reach cargo rather than tm's parser.
pub fn wrapper_args(argv: impl IntoIterator<Item = OsString>) -> Option<Vec<OsString>> {
    let mut argv = argv.into_iter().skip(1);
    if argv.next()? != WRAPPER_ARG {
        return None;
    }
    Some(argv.collect())
}

/// `tm __cargo-wrap <args>`: [`run_wrapped`] with [`WrapperEnv`] read from
/// this process's environment. Exits `127` when the real cargo cannot be
/// found or spawned.
pub fn run_wrapper(args: Vec<OsString>) -> ExitCode {
    let env = WrapperEnv {
        slots: std::env::var(SLOTS_ENV)
            .ok()
            .and_then(|value| value.parse().ok()),
        lock_dir: std::env::var_os(LOCK_DIR_ENV).map(PathBuf::from),
        shim_dir: std::env::var_os(SHIM_DIR_ENV).map(PathBuf::from),
        path: std::env::var_os("PATH"),
        held: std::env::var_os(HELD_ENV).is_some(),
        jobs_set: std::env::var_os("CARGO_BUILD_JOBS").is_some(),
        cores: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
    };
    match run_wrapped(&args, &env) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            eprintln!("tm: cargo wrapper: {err}");
            ExitCode::from(127)
        }
    }
}

/// Quote `s` as one POSIX shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::tempdir;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn write_exe(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn wrapper_args_takes_everything_after_the_wrapper_arg() {
        assert_eq!(
            wrapper_args(args(&["tm", "__cargo-wrap", "build", "--help"])),
            Some(args(&["build", "--help"]))
        );
        assert_eq!(
            wrapper_args(args(&["tm", "__cargo-wrap"])),
            Some(Vec::new())
        );
        assert_eq!(wrapper_args(args(&["tm", "work", "run"])), None);
        assert_eq!(wrapper_args(args(&["tm"])), None);
    }

    #[test]
    fn compiling_subcommands_need_a_slot() {
        for cmd in [
            &["build"][..],
            &["b"],
            &["test", "--all"],
            &["check"],
            &["clippy", "--all-targets", "--", "-D", "warnings"],
            &["run", "--release"],
            &["doc"],
            &["install", "ripgrep"],
            &["nextest", "run"],
        ] {
            assert!(needs_slot(&args(cmd)), "{cmd:?} should take a slot");
        }
    }

    #[test]
    fn global_options_are_skipped_to_find_the_subcommand() {
        assert!(needs_slot(&args(&["+nightly", "build"])));
        assert!(needs_slot(&args(&["-v", "--locked", "test"])));
        assert!(needs_slot(&args(&["--config", "build.jobs=2", "build"])));
        assert!(needs_slot(&args(&[
            "--color", "never", "-Z", "unstable", "check"
        ])));
        assert!(needs_slot(&args(&["-C", "/some/dir", "build"])));
        assert!(!needs_slot(&args(&["-q", "fmt", "--check"])));
        assert!(!needs_slot(&args(&["--color=never", "metadata"])));
    }

    #[test]
    fn non_compiling_commands_skip_the_slot() {
        for cmd in [
            &[][..],
            &["fmt", "--check"],
            &["metadata", "--format-version", "1"],
            &["--version"],
            &["-V"],
            &["--list"],
            &["-h"],
            &["help", "build"],
            &["tree"],
            &["clean"],
            &["locate-project"],
            &["new", "foo"],
            &["add", "serde"],
            &["update"],
            &["fetch"],
        ] {
            assert!(!needs_slot(&args(cmd)), "{cmd:?} should skip the slot");
        }
    }

    #[test]
    fn jobs_split_cores_across_slots_never_below_one() {
        assert_eq!(jobs_per_slot(14, 4), 3);
        assert_eq!(jobs_per_slot(16, 2), 8);
        assert_eq!(jobs_per_slot(2, 4), 1);
        assert_eq!(jobs_per_slot(0, 1), 1);
        assert_eq!(jobs_per_slot(8, 0), 8);
    }

    #[test]
    fn shim_script_execs_tm_with_the_wrapper_arg_and_original_args() {
        let script = shim_script(Path::new("/opt/it's/tm"));
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(
            script.contains(r#"exec '/opt/it'\''s/tm' __cargo-wrap "$@""#),
            "{script}"
        );
    }

    #[test]
    fn deploy_shim_writes_an_executable_cargo_and_the_lock_dir() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("build-slots");
        let shim_dir = deploy_shim(&root, Path::new("/usr/bin/tm")).unwrap();
        assert_eq!(shim_dir, root.join("bin"));
        let shim = shim_dir.join("cargo");
        assert_eq!(
            std::fs::read_to_string(&shim).unwrap(),
            shim_script(Path::new("/usr/bin/tm"))
        );
        let mode = std::fs::metadata(&shim).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "shim must be executable");
        assert!(root.join("locks").is_dir());

        // Redeploying over an existing shim succeeds and repoints it.
        deploy_shim(&root, Path::new("/new/tm")).unwrap();
        assert!(std::fs::read_to_string(&shim).unwrap().contains("/new/tm"));
    }

    #[test]
    fn lane_env_puts_the_shim_first_on_path_once() {
        let env = lane_env(
            3,
            Path::new("/h/slots"),
            Path::new("/h/slots/bin"),
            Some(OsStr::new("/h/slots/bin:/usr/bin:/bin")),
        );
        let get = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{key} missing from {env:?}"))
        };
        assert_eq!(get("PATH"), "/h/slots/bin:/usr/bin:/bin");
        assert_eq!(get(SLOTS_ENV), "3");
        assert_eq!(get(LOCK_DIR_ENV), "/h/slots/locks");
        assert_eq!(get(SHIM_DIR_ENV), "/h/slots/bin");
    }

    #[test]
    fn lane_env_with_no_current_path_is_just_the_shim_dir() {
        let env = lane_env(1, Path::new("/r"), Path::new("/r/bin"), None);
        assert!(env.contains(&("PATH".to_string(), "/r/bin".to_string())));
    }

    #[test]
    fn find_real_cargo_skips_the_shim_dir() {
        let tmp = tempdir().unwrap();
        let shim_dir = tmp.path().join("shim");
        let real_dir = tmp.path().join("real");
        let empty_dir = tmp.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        write_exe(&shim_dir.join("cargo"), "#!/bin/sh\n");
        write_exe(&real_dir.join("cargo"), "#!/bin/sh\n");
        let path = std::env::join_paths([&shim_dir, &empty_dir, &real_dir]).unwrap();

        assert_eq!(
            find_real_cargo(&path, &shim_dir),
            Some(real_dir.join("cargo"))
        );
        let only_shim = std::env::join_paths([&shim_dir]).unwrap();
        assert_eq!(find_real_cargo(&only_shim, &shim_dir), None);
    }

    #[test]
    fn find_real_cargo_ignores_a_non_executable_cargo() {
        let tmp = tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("cargo"), "not a program").unwrap();
        let path = std::env::join_paths([&plain]).unwrap();
        assert_eq!(find_real_cargo(&path, Path::new("/nowhere")), None);
    }

    #[test]
    fn acquire_slot_waits_for_a_held_slot_and_reports_queueing_once() {
        let tmp = tempdir().unwrap();
        let lock_dir = tmp.path().to_path_buf();
        let held = acquire_slot(&lock_dir, 1, &mut || panic!("first take must not queue")).unwrap();

        let (queued_tx, queued_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let dir = lock_dir.clone();
        let waiter = std::thread::spawn(move || {
            let mut calls = 0;
            let slot = acquire_slot(&dir, 1, &mut || {
                calls += 1;
                queued_tx.send(()).unwrap();
            })
            .unwrap();
            done_tx.send(()).unwrap();
            drop(slot);
            calls
        });

        queued_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter should report it is queued");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(600)).is_err(),
            "waiter must not get the slot while it is held"
        );
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter should get the slot once it is released");
        assert_eq!(
            waiter.join().unwrap(),
            1,
            "queued line printed exactly once"
        );
    }

    #[test]
    fn acquire_slot_hands_out_distinct_slots_up_to_n() {
        let tmp = tempdir().unwrap();
        let _a = acquire_slot(tmp.path(), 2, &mut || panic!("slot 1 free")).unwrap();
        let _b = acquire_slot(tmp.path(), 2, &mut || panic!("slot 2 free")).unwrap();
    }

    /// A fake real cargo that records its args and `CARGO_BUILD_JOBS`/held
    /// marker to `record` and exits `code`.
    fn fake_cargo(dir: &Path, record: &Path, code: i32) {
        write_exe(
            &dir.join("cargo"),
            &format!(
                "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$*\" \"${{CARGO_BUILD_JOBS:-unset}}\" \"${{{HELD_ENV}:-unset}}\" > '{}'\nexit {code}\n",
                record.display()
            ),
        );
    }

    fn wrapper_env(tmp: &Path, real_dir: &Path, slots: u32) -> WrapperEnv {
        let shim_dir = tmp.join("shim");
        write_exe(&shim_dir.join("cargo"), "#!/bin/sh\nexit 99\n");
        WrapperEnv {
            slots: Some(slots),
            lock_dir: Some(tmp.join("locks")),
            path: Some(std::env::join_paths([&shim_dir, &real_dir.to_path_buf()]).unwrap()),
            shim_dir: Some(shim_dir),
            held: false,
            jobs_set: false,
            cores: 12,
        }
    }

    #[test]
    fn run_wrapped_passes_exit_code_through_and_sets_jobs_for_a_build() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("real");
        let record = tmp.path().join("record");
        fake_cargo(&real, &record, 3);
        let env = wrapper_env(tmp.path(), &real, 4);

        let code = run_wrapped(&args(&["build", "--release"]), &env).unwrap();

        assert_eq!(code, 3);
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            "build --release|3|1\n"
        );
    }

    #[test]
    fn run_wrapped_leaves_an_explicit_jobs_setting_alone() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("real");
        let record = tmp.path().join("record");
        fake_cargo(&real, &record, 0);
        let mut env = wrapper_env(tmp.path(), &real, 4);
        env.jobs_set = true;

        assert_eq!(run_wrapped(&args(&["test"]), &env).unwrap(), 0);
        // The fake cargo inherits the test process's env, so only check the
        // wrapper did not overwrite it with cores / slots.
        let recorded = std::fs::read_to_string(&record).unwrap();
        assert!(!recorded.starts_with("test|3|"), "{recorded}");
    }

    #[test]
    fn run_wrapped_skips_the_slot_for_fmt_even_when_every_slot_is_held() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("real");
        let record = tmp.path().join("record");
        fake_cargo(&real, &record, 0);
        let env = wrapper_env(tmp.path(), &real, 1);
        std::fs::create_dir_all(tmp.path().join("locks")).unwrap();
        let _held = acquire_slot(&tmp.path().join("locks"), 1, &mut || {}).unwrap();

        assert_eq!(run_wrapped(&args(&["fmt", "--check"]), &env).unwrap(), 0);
        assert!(
            std::fs::read_to_string(&record)
                .unwrap()
                .starts_with("fmt --check|")
        );
    }

    #[test]
    fn run_wrapped_skips_the_slot_when_nested_inside_a_held_one() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("real");
        let record = tmp.path().join("record");
        fake_cargo(&real, &record, 0);
        let mut env = wrapper_env(tmp.path(), &real, 1);
        env.held = true;
        std::fs::create_dir_all(tmp.path().join("locks")).unwrap();
        let _held = acquire_slot(&tmp.path().join("locks"), 1, &mut || {}).unwrap();

        assert_eq!(run_wrapped(&args(&["build"]), &env).unwrap(), 0);
    }

    #[test]
    fn run_wrapped_without_slots_runs_straight_through() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("real");
        let record = tmp.path().join("record");
        fake_cargo(&real, &record, 0);
        let mut env = wrapper_env(tmp.path(), &real, 0);
        env.slots = None;
        env.lock_dir = None;

        assert_eq!(run_wrapped(&args(&["build"]), &env).unwrap(), 0);
        assert!(!tmp.path().join("locks").exists());
    }

    #[test]
    fn run_wrapped_errors_when_no_real_cargo_is_on_path() {
        let tmp = tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let env = wrapper_env(tmp.path(), &empty, 1);

        let err = run_wrapped(&args(&["build"]), &env).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn run_wrapped_reports_a_signal_death_as_128_plus_signal() {
        let tmp = tempdir().unwrap();
        let real = tmp.path().join("real");
        write_exe(&real.join("cargo"), "#!/bin/sh\nkill -TERM $$\n");
        let env = wrapper_env(tmp.path(), &real, 1);

        assert_eq!(run_wrapped(&args(&["build"]), &env).unwrap(), 128 + 15);
    }
}
