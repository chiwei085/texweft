mod scan;
mod watch;

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::fs::File;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, thread};
use scan::Fls;

/// Engine runs per invocation, counting the first (latexmk's `$max_repeat`).
const MAX_ENGINE_RUNS: u32 = 5;
const USAGE: &str = "usage: texweft build <main.tex> [options]
       texweft watch <main.tex> [options] [--poll]
       texweft clean <main.tex> [--outdir DIR] [--lock-timeout SECS]
       texweft --version
options: --engine pdflatex|xelatex|lualatex  --message-format=json|human  --force
         --outdir DIR (default build, relative to the main file's directory)
         --step-timeout SECS  --lock-timeout SECS";
/// Where outputs go unless `--outdir` says otherwise, relative to the main file's directory.
const DEFAULT_OUTDIR: &str = "build";
/// Output subdirectories one build may create for `\include` targets.
const MAX_CREATED_DIRS: usize = 64;
/// How long a one-shot `build` or `clean` waits for another texweft using the same outputs.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace period between SIGTERM and SIGKILL for a tool's process group.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// The tool being run, so a termination signal can stop it before texweft exits.
static RUNNING: Mutex<Option<Child>> = Mutex::new(None);
/// Set once texweft is terminating: no tool starts and no message is written after it.
static STOPPING: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct Opts {
    engine: String,
    /// Relative to the main file's directory, not to the caller's working directory.
    outdir: PathBuf,
    json: bool,
    poll: bool,
    step_timeout: Option<Duration>,
    /// `None` waits indefinitely (watch).
    lock_timeout: Option<Duration>,
}

/// Stops the running tool's whole process tree and exits. Partial builds leave no state file.
fn shutdown(code: i32) -> ! {
    STOPPING.store(true, Ordering::SeqCst);
    if let Some(c) = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).take() {
        stop_tree(c);
    }
    std::process::exit(code)
}

/// Terminates a tool and everything it started (mktexpk, mf, ...): the tool leads its own process
/// group, which gets SIGTERM, then SIGKILL after a grace period.
fn stop_tree(mut c: Child) {
    #[cfg(unix)]
    {
        let group = -(c.id() as i32);
        unsafe { libc::kill(group, libc::SIGTERM) };
        let t = Instant::now();
        while t.elapsed() < KILL_GRACE && !matches!(c.try_wait(), Ok(Some(_))) {
            thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
    }
    // ponytail: on Windows only the tool itself is killed; its helpers need a Job Object.
    let _ = c.kill();
    let _ = c.wait();
}

/// Blocks the calling thread for good; used once `shutdown` owns the process.
fn park() -> ! {
    loop {
        thread::park();
    }
}

fn emit(json: bool, ty: &str, fields: Value) {
    if STOPPING.load(Ordering::SeqCst) {
        park();
    }
    let mut m = Map::new();
    m.insert("v".into(), 0.into());
    m.insert("type".into(), ty.into());
    if let Value::Object(f) = fields {
        m.extend(f);
    }
    let m = Value::Object(m);
    if json {
        // A closed stdout means the consumer is gone; don't keep building for nobody.
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{m}").and_then(|_| out.flush()).is_err() {
            shutdown(141);
        }
    } else {
        human(&m);
    }
}

/// Takes the advisory lock guarding the outputs at `path`. The OS releases it if texweft dies.
/// `Err` when `timeout` passes first; `Ok(None)` when the filesystem cannot lock (build unlocked).
fn acquire(path: &Path, timeout: Option<Duration>, mut waiting: impl FnMut(Option<u32>)) -> Result<Option<File>, ()> {
    let Ok(mut f) = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path) else {
        return Ok(None);
    };
    let t = Instant::now();
    let mut announced = false;
    loop {
        match f.try_lock() {
            Ok(()) => {
                let _ = f.set_len(0);
                let _ = f.write_all(std::process::id().to_string().as_bytes());
                return Ok(Some(f));
            }
            Err(fs::TryLockError::WouldBlock) => {
                if !announced {
                    announced = true;
                    waiting(read(path).trim().parse().ok());
                }
                if timeout.is_some_and(|d| t.elapsed() >= d) {
                    return Err(());
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// Engine outputs that are never read back by the next run.
const NOT_FEEDBACK: [&str; 10] = [".log", ".pdf", ".xdv", ".bcf", ".run.xml", ".idx", ".glo", ".acn", ".ist", "synctex"];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut it = args.iter();
    let usage = |msg: &str| {
        eprintln!("texweft: {msg}\n{USAGE}");
        ExitCode::from(2)
    };
    let cmd = it.next().map(String::as_str);
    if matches!(cmd, Some("--version" | "-V")) {
        println!("texweft {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if !matches!(cmd, Some("build" | "watch" | "clean")) {
        return usage("expected the `build`, `watch`, or `clean` command");
    }
    let watching = cmd == Some("watch");
    let lock_timeout = if watching { None } else { Some(DEFAULT_LOCK_TIMEOUT) };
    let mut o = Opts { engine: "pdflatex".into(), outdir: DEFAULT_OUTDIR.into(), json: false, poll: false, step_timeout: None, lock_timeout };
    let (mut main, mut force) = (None, false);
    while let Some(a) = it.next() {
        let (flag, inline) = a.split_once('=').map_or((a.as_str(), None), |(f, v)| (f, Some(v.to_string())));
        let mut value = || inline.clone().or_else(|| it.next().cloned());
        let secs = |v: Option<String>| v.and_then(|v| v.parse::<f64>().ok()).filter(|s| *s > 0.0).map(Duration::from_secs_f64);
        match flag {
            "--engine" => o.engine = value().unwrap_or_default(),
            "--message-format" => match value().as_deref() {
                Some("json") => o.json = true,
                Some("human") => o.json = false,
                v => return usage(&format!("unknown message format `{}`", v.unwrap_or(""))),
            },
            "--force" => force = true,
            "--outdir" => match value() {
                Some(d) if !d.is_empty() => o.outdir = d.into(),
                _ => return usage("--outdir takes a directory"),
            },
            "--poll" if watching => o.poll = true,
            "--step-timeout" => match secs(value()) {
                Some(d) => o.step_timeout = Some(d),
                None => return usage("--step-timeout takes a positive number of seconds"),
            },
            "--lock-timeout" => match secs(value()) {
                Some(d) => o.lock_timeout = Some(d),
                None => return usage("--lock-timeout takes a positive number of seconds"),
            },
            s if !s.starts_with('-') && main.is_none() => main = Some(a.clone()),
            _ => return usage(&format!("unexpected argument `{a}`")),
        }
    }
    if !["pdflatex", "xelatex", "lualatex"].contains(&o.engine.as_str()) {
        return usage(&format!("unsupported engine `{}`", o.engine));
    }
    let Some(main) = main.as_deref().and_then(|m| fs::canonicalize(m).ok()) else {
        return usage("main file not found");
    };
    // Exit status 128 + signal, the shell convention: 130 SIGINT, 143 SIGTERM, 129 SIGHUP.
    #[cfg(unix)]
    match signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM, libc::SIGHUP]) {
        Ok(mut signals) => {
            thread::spawn(move || {
                if let Some(sig) = signals.forever().next() {
                    shutdown(128 + sig);
                }
            });
        }
        Err(e) => eprintln!("texweft: cannot install signal handlers: {e}"),
    }
    if cmd == Some("clean") {
        return if clean(&main, &o) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    if watching {
        watch::run(&main, &o, force);
    }
    if Build::new(&main, &o, force).run().ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// What one build saw, for `watch` to decide when the next one is due.
pub struct Report {
    pub ok: bool,
    /// Inputs and the content hash the build used; `None` when unknown (saved mid-build).
    pub inputs: Vec<(PathBuf, Option<String>)>,
    /// When those hashes were verified: a file modified later may differ from its hash.
    pub verified: SystemTime,
    /// Paths whose appearance may fix the build: missing files, missing programs.
    pub awaited: Vec<PathBuf>,
}

struct Build {
    main: PathBuf,
    awaited: Vec<PathBuf>,
    /// The main file's directory: the engine's working directory, where sources resolve.
    dir: PathBuf,
    /// Where every output goes (`-output-directory`), absolute once `run` has created it.
    out: PathBuf,
    job: String,
    o: Opts,
    bin: Option<PathBuf>,
    force: bool,
    step: u32,
    /// LaTeX reads a `.bbl` shipped next to the main file, not one a bibliography tool made: it
    /// is then an input like any source, even when it sits at the output path (`--outdir .`).
    shipped_bbl: bool,
    fp: std::cell::RefCell<Fingerprints>,
    /// The `diagnostic` and `missing` messages this build reported, replayed when it is reused.
    reported: std::cell::RefCell<Vec<Value>>,
    /// A tool failed for a reason other than the document (crash, timeout, a tool's own error),
    /// so the same inputs may not fail again: such a build is not reused.
    tool_trouble: std::cell::Cell<bool>,
}

struct Ran {
    /// The program could not be started at all.
    not_found: bool,
    /// Stopped after `--step-timeout`.
    timed_out: bool,
    code: Option<i32>,
    ms: u128,
    output: String,
}

fn fnv(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

// ponytail: FNV-1a detects edits, not adversarial collisions; switch to SHA-256 if hashes leave the machine.
fn hash(p: &Path) -> Option<String> {
    fs::read(p).ok().map(|b| fnv(&b))
}

/// How long after its last change a file's stat is trusted to reflect its content: a write within
/// the same timestamp tick as an earlier read leaves the stat unchanged (git's "racy clean").
/// Covers coarse filesystem clocks (FAT: 2 s).
const RACY: Duration = Duration::from_secs(3);

/// A file's identity, size, and change times, and the newest of those times. Any write changes
/// the ctime, which `touch -d`, `cp -p`, and unpacking archives cannot set back.
fn stat(p: &Path) -> Option<(String, SystemTime)> {
    let m = fs::metadata(p).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ctime = SystemTime::UNIX_EPOCH + Duration::new(m.ctime().max(0) as u64, m.ctime_nsec() as u32);
        let key = format!("{}.{}:{}.{}:{}:{}:{}", m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.size(), m.ino(), m.dev());
        Some((key, m.modified().ok()?.max(ctime)))
    }
    #[cfg(not(unix))]
    {
        let t = m.modified().ok()?;
        Some((format!("{:?}:{}", t, m.len()), t))
    }
}

/// Content hashes, reused while a file's stat is unchanged, as latexmk and git do: an unchanged
/// file is never read again. Only stats older than `RACY` at hashing time are kept.
#[derive(Default)]
struct Fingerprints {
    known: BTreeMap<PathBuf, (String, Option<String>)>,
    /// Whether an entry was added or replaced since loading.
    dirty: bool,
}

impl Fingerprints {
    fn load(v: &Value) -> Fingerprints {
        let known = v.as_object().into_iter().flatten().filter_map(|(p, e)| {
            Some((PathBuf::from(p), (e[0].as_str()?.to_string(), e[1].as_str().map(String::from))))
        });
        Fingerprints { known: known.collect(), dirty: false }
    }

    fn save<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> Value {
        let m = paths.into_iter().filter_map(|p| Some((p.display().to_string(), json!(self.known.get(p)?))));
        Value::Object(m.collect())
    }

    fn hash(&mut self, p: &Path) -> Option<String> {
        let before = stat(p);
        if let (Some((key, _)), Some((k, h))) = (&before, self.known.get(p))
            && key == k
        {
            return h.clone();
        }
        let h = hash(p);
        match (before, stat(p)) {
            (Some((a, t)), Some((b, _))) if a == b && t + RACY < SystemTime::now() => {
                self.known.insert(p.to_path_buf(), (a, h.clone()));
                self.dirty = true;
            }
            _ => self.dirty |= self.known.remove(p).is_some(),
        }
        h
    }
}

/// Enough to tell whether a tool rewrote an artifact: writing changes the mtime.
fn stamp(p: &Path) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The first line a command prints (kpsewhich answers this way), if it ran and printed one.
fn first_line(c: &mut Command) -> Option<String> {
    let o = c.output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).lines().next()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Bumped whenever the state gains something an up-to-date build must report; an older state
/// then counts as no previous run.
const STATE_VERSION: u64 = 3;

fn read_state(p: &Path) -> Option<Value> {
    fs::read(p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(|s| s["v"] == STATE_VERSION)
}

fn read(p: &Path) -> String {
    fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn find_bin(engine: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find(|d| d.join(engine).is_file())
}

fn changed(files: Vec<Value>) -> Option<Value> {
    (!files.is_empty()).then(|| json!({"kind": "inputs-changed", "files": files}))
}

/// An `.aux` and every `.aux` it `\@input`s (include targets, as written), each visited once:
/// the targets, and the other lines of all of them.
fn aux_tree(aux: &Path, targets: &mut Vec<String>, lines: &mut Vec<String>) {
    for l in read(aux).lines() {
        match l.strip_prefix("\\@input{").and_then(|r| r.strip_suffix('}')) {
            Some(sub) if !targets.iter().any(|t| t == sub) => {
                targets.push(sub.to_string());
                aux_tree(&aux.with_file_name(sub), targets, lines);
            }
            Some(_) => {}
            None => lines.push(l.to_string()),
        }
    }
}

/// Runs a tool to completion in its own process group, reading its output, while letting
/// `shutdown` or `timeout` stop the whole group. Returns the exit code, output, and whether it timed out.
fn run_tool(cmd: &mut Command, timeout: Option<Duration>) -> std::io::Result<(Option<i32>, String, bool)> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if STOPPING.load(Ordering::SeqCst) {
        drop(running);
        park();
    }
    let mut child = cmd.spawn()?;
    let drain = |mut r: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            b
        })
    };
    let (out, err) = (drain(Box::new(child.stdout.take().unwrap())), drain(Box::new(child.stderr.take().unwrap())));
    let exited = ExitWatch::new(child.id());
    *running = Some(child);
    drop(running);
    let t = Instant::now();
    let (code, timed_out) = loop {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        let Some(c) = running.as_mut() else {
            drop(running);
            park(); // shutdown took it
        };
        if let Some(status) = c.try_wait()? {
            let pid = c.id() as i32;
            running.take();
            // Helpers that outlived the tool would hold its pipes open.
            #[cfg(unix)]
            unsafe {
                libc::kill(-pid, libc::SIGKILL)
            };
            let _ = pid;
            break (status.code(), false);
        }
        if timeout.is_some_and(|d| t.elapsed() >= d) {
            let c = running.take().unwrap();
            drop(running);
            stop_tree(c);
            break (None, true);
        }
        drop(running);
        exited.wait(timeout.map_or(Duration::MAX, |d| d.saturating_sub(t.elapsed())));
    };
    let text = |t: thread::JoinHandle<Vec<u8>>| String::from_utf8_lossy(&t.join().unwrap_or_default()).into_owned();
    Ok((code, text(out) + &text(err), timed_out))
}

/// Waits for a child to exit without polling it: a pidfd becomes readable when it does (also
/// when `shutdown` kills it). Without pidfds (not Linux, kernel < 5.3), short sleeps instead.
struct ExitWatch(i32);

impl ExitWatch {
    fn new(pid: u32) -> ExitWatch {
        #[cfg(target_os = "linux")]
        return ExitWatch(unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as i32);
        #[cfg(not(target_os = "linux"))]
        return { let _ = pid; ExitWatch(-1) };
    }

    /// Returns once the child may have exited, or after at most `max`.
    fn wait(&self, max: Duration) {
        #[cfg(target_os = "linux")]
        if self.0 >= 0 {
            let mut p = libc::pollfd { fd: self.0, events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, max.min(Duration::from_secs(1)).as_millis().max(1) as i32) };
            return;
        }
        thread::sleep(max.min(Duration::from_millis(5)));
    }
}

impl Drop for ExitWatch {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

impl Build {
    fn new(main: &Path, o: &Opts, force: bool) -> Build {
        Build {
            main: main.to_path_buf(),
            awaited: Vec::new(),
            dir: main.parent().unwrap().to_path_buf(),
            out: main.parent().unwrap().join(&o.outdir),
            job: main.file_stem().unwrap().to_string_lossy().into(),
            bin: find_bin(&o.engine),
            o: o.clone(),
            force,
            step: 0,
            shipped_bbl: false,
            fp: Default::default(),
            reported: Default::default(),
            tool_trouble: Default::default(),
        }
    }

    fn hash(&self, p: &Path) -> Option<String> {
        self.fp.borrow_mut().hash(p)
    }

    /// Hashes of `paths` as they are now.
    fn hashes<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) -> BTreeMap<PathBuf, Option<String>> {
        paths.into_iter().map(|p| (p.clone(), self.hash(p))).collect()
    }

    fn path(&self, ext: &str) -> PathBuf {
        self.out.join(format!("{}.{ext}", self.job))
    }

    fn lock_path(&self) -> PathBuf {
        self.out.join(format!(".{}.texweft.lock", self.job))
    }

    /// Where a `.bbl` shipped with the sources sits (next to the main file).
    fn shipped_bbl_path(&self) -> PathBuf {
        self.dir.join(format!("{}.bbl", self.job))
    }

    /// The `.aux` tree's include targets and `\citation`/`\bibdata`/`\bibstyle` lines.
    fn aux(&self) -> (Vec<String>, String) {
        let (mut targets, mut lines) = (Vec::new(), Vec::new());
        aux_tree(&self.path("aux"), &mut targets, &mut lines);
        lines.retain(|l| ["\\citation", "\\bibdata", "\\bibstyle"].iter().any(|p| l.starts_with(p)));
        (targets, lines.join("\n"))
    }

    /// A tool invocation: run from the source directory, so relative paths in the document mean
    /// what they mean to any other LaTeX toolchain.
    fn cmd(&self, tool: &str) -> Command {
        let mut c = Command::new(self.exe(tool));
        c.current_dir(&self.dir).env("max_print_line", "10000");
        c
    }

    /// BibTeX runs next to the `.aux` it reads (so `\@input{ch/one.aux}` resolves), and finds
    /// databases and styles in the source directory first, then on the usual search path.
    fn bibtex_env(&self, c: &mut Command) {
        for var in ["BIBINPUTS", "BSTINPUTS"] {
            // An empty rest still ends the list with a separator: kpathsea's default path.
            let rest = env::var_os(var).unwrap_or_default();
            if let Ok(v) = env::join_paths(std::iter::once(self.dir.clone()).chain(env::split_paths(&rest))) {
                c.env(var, v);
            }
        }
    }

    /// Output directories for `\include` targets the `.aux` tree names: TeX cannot create them.
    /// Only plain relative paths inside the output directory, so a document cannot steer this.
    fn include_dirs(&self) -> Vec<PathBuf> {
        self.aux()
            .0
            .iter()
            .map(Path::new)
            .filter(|t| t.components().count() > 1 && t.components().all(|c| matches!(c, std::path::Component::Normal(_))))
            .filter_map(|t| t.parent().map(|d| self.out.join(d)))
            .filter(|d| !d.is_dir())
            .collect()
    }

    fn exe(&self, name: &str) -> PathBuf {
        self.bin.as_ref().map(|b| b.join(name)).filter(|p| p.is_file()).unwrap_or_else(|| name.into())
    }

    fn emit(&self, ty: &str, fields: Value) {
        if ty == "diagnostic" || ty == "missing" {
            let mut m = fields.clone();
            m["type"] = ty.into();
            self.reported.borrow_mut().push(m);
        }
        emit(self.o.json, ty, fields);
    }

    /// kpathsea's SELFAUTOPARENT. kpathsea derives it from where its binary really is (symlinks
    /// resolved, three levels up), so it is worked out the same way instead of starting kpsewhich
    /// on every build; a kpsewhich that is a script (a wrapper) may run one elsewhere and is asked.
    fn tex_root(&self) -> Option<PathBuf> {
        let bin = self.bin.as_ref()?;
        let kpse = fs::canonicalize(bin.join("kpsewhich")).ok();
        let binary = kpse.as_ref().is_some_and(|k| {
            let mut magic = [0; 2];
            File::open(k).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && magic != *b"#!"
        });
        let root = match kpse {
            Some(k) if binary => k.ancestors().nth(3).map(Path::to_path_buf),
            _ => first_line(Command::new(bin.join("kpsewhich")).arg("-var-value=SELFAUTOPARENT")).map(PathBuf::from),
        };
        fs::canonicalize(root.or_else(|| Some(bin.parent()?.parent()?.to_path_buf()))?).ok()
    }

    fn start_step(&mut self, tool: &str, mut cmd: Command, rerun: Value) -> Ran {
        self.step += 1;
        self.emit("step-started", json!({"step": self.step, "tool": tool, "rerun": rerun}));
        let t = Instant::now();
        let out = run_tool(&mut cmd, self.o.step_timeout);
        Ran {
            not_found: out.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            timed_out: out.as_ref().is_ok_and(|o| o.2),
            code: out.as_ref().ok().and_then(|o| o.0),
            ms: t.elapsed().as_millis(),
            output: out.map(|o| o.1).unwrap_or_else(|e| format!("{tool}: {e}")),
        }
    }

    fn finish_step(&self, tool: &str, ran: &Ran, log: &Path) {
        let log = log.exists().then_some(log);
        self.emit("step-finished", json!({"step": self.step, "tool": tool, "exit_code": ran.code, "duration_ms": ran.ms, "log": log}));
    }

    /// A failing tool whose log explains nothing still gets an error, with its own output as `raw`.
    fn tool_failed(&self, tool: &str, ran: &Ran) {
        self.tool_trouble.set(true);
        let lines: Vec<&str> = ran.output.lines().collect();
        let raw = &lines[lines.len().saturating_sub(20)..];
        let message = match (ran.timed_out, ran.code) {
            (true, _) => format!("{tool} did not finish within --step-timeout and was stopped"),
            (_, Some(c)) => format!("{tool} exited with status {c}"),
            (_, None) => format!("{tool} did not run"),
        };
        let mut msg = json!({"step": self.step, "level": "error"});
        if ran.timed_out {
            msg["code"] = "timeout".into();
        }
        msg["message"] = message.into();
        msg["raw"] = json!(raw);
        self.emit("diagnostic", msg);
    }

    /// A tool that could not start: not installed, or missing a shared library.
    /// Reported as `missing` so the caller can install it; true if so.
    fn tool_missing(&mut self, tool: &str, ran: &Ran) -> bool {
        let m = if ran.not_found {
            let raw = vec![ran.output.lines().next().unwrap_or_default().to_string()];
            scan::Missing { kind: scan::Kind::Program, name: tool.to_string(), fatal: true, substitute: None, raw }
        } else if let Some(m) = scan::missing_library(&ran.output) {
            m
        } else {
            return false;
        };
        self.emit_missing(self.step, &[m]);
        true
    }

    fn emit_missing(&mut self, step: u32, missing: &[scan::Missing]) {
        for m in missing.iter().filter(|m| m.fatal) {
            match m.kind {
                scan::Kind::File => self.awaited.push(self.dir.join(&m.name)),
                scan::Kind::Program => {
                    let path = env::var_os("PATH").unwrap_or_default();
                    self.awaited.extend(env::split_paths(&path).map(|d| d.join(&m.name)));
                }
                _ => {} // fonts and libraries: installed by the caller, seen through ls-R
            }
        }
        for m in missing {
            let kind = match m.kind {
                scan::Kind::File => "file",
                scan::Kind::FontFile => "font-file",
                scan::Kind::FontName => "font-name",
                scan::Kind::FontOutline => "font-outline",
                scan::Kind::Program => "program",
                scan::Kind::Library => "library",
            };
            let mut msg = json!({"step": step, "kind": kind, "name": m.name, "fatal": m.fatal});
            if let Some(s) = m.substitute {
                msg["substitute"] = s.into();
            }
            msg["raw"] = json!(m.raw);
            self.emit("missing", msg);
        }
    }

    /// Whether the last converged build is still current. `now` holds the current hashes of its
    /// inputs (`prev`), taken once and reused to verify the first step.
    fn initial_rerun(&self, state: Option<&Value>, root: &Path, prev: &[(PathBuf, Option<String>, String)], now: &BTreeMap<PathBuf, Option<String>>) -> Option<Value> {
        if self.force {
            return Some(json!({"kind": "forced"}));
        }
        let no_prev = Some(json!({"kind": "no-previous-run"}));
        let Some(s) = state else { return no_prev };
        if s["tex_root"] != json!(root) {
            return Some(json!({"kind": "toolchain-changed", "was": s["tex_root"], "now": root}));
        }
        if s["engine"] != self.o.engine.as_str() {
            return Some(json!({"kind": "toolchain-changed", "was": s["engine"], "now": self.o.engine}));
        }
        if s["pdf"].as_str() != self.hash(&self.path("pdf")).as_deref() {
            return no_prev;
        }
        let files = prev.iter().filter_map(|(p, was, _)| {
            let now = now.get(p).cloned().flatten();
            (*was != now).then(|| json!({"path": p, "was": was, "now": now}))
        });
        changed(files.collect())
    }

    /// An engine output whose change can matter to the next run: inside the output directory
    /// (caches the engine writes elsewhere, luaotfload under texmf-var, are not document state)
    /// and not one that is never read back.
    fn may_feed_back(&self, p: &Path) -> bool {
        let name = p.to_string_lossy();
        p.starts_with(&self.out) && !NOT_FEEDBACK.iter().any(|e| name.ends_with(e) || (*e == "synctex" && name.contains(e)))
    }

    /// Outputs the next engine run reads back and that this run changed.
    fn feedback(&self, fls: &Fls, before: &BTreeMap<PathBuf, Option<String>>) -> Vec<Value> {
        let mut files = Vec::new();
        for p in fls.outputs.iter().filter(|p| self.may_feed_back(p)) {
            let (was, now) = (before.get(p).cloned().flatten(), self.hash(p));
            // Read this run: did it change? Not read: only a newly created file may be read next run.
            let rerun = if fls.inputs.contains(p) { was != now } else { was.is_none() && now.is_some() };
            if rerun {
                files.push(json!({"path": p, "was": was, "now": now}));
            }
        }
        files
    }

    /// What one engine run read: `.fls` inputs that no step of the build writes.
    fn engine_inputs(&self, fls: &Fls) -> Vec<PathBuf> {
        let made = (!self.shipped_bbl).then(|| self.path("bbl"));
        fls.inputs.iter().filter(|p| !fls.outputs.contains(*p) && Some(*p) != made.as_ref()).cloned().collect()
    }

    /// Inputs that may have changed while a step ran, so its result may not match them: a known
    /// input whose hash differs from `known` (its hash when last verified, at the latest just
    /// before the step), or an input first seen in this step whose mtime falls inside it. Hashes
    /// before and after cannot prove the tool read one consistent version, but any change they
    /// reveal is enough to distrust the result. `known` takes the new hashes, so each input is
    /// hashed once per step.
    fn changed_during(&self, read: &[PathBuf], known: &mut BTreeMap<PathBuf, Option<String>>, from: SystemTime) -> Vec<Value> {
        let to = SystemTime::now();
        let mut files = Vec::new();
        for p in read {
            let now = self.hash(p);
            let changed = match known.get(p) {
                Some(was) => (*was != now).then(|| json!({"path": p, "was": was, "now": now})),
                // First seen in this step: modified inside it, or already gone again.
                None => match fs::metadata(p).and_then(|m| m.modified()) {
                    Ok(t) => (t >= from && t <= to).then(|| json!({"path": p, "was": null, "now": now})),
                    Err(_) => Some(json!({"path": p, "was": null, "now": null})),
                },
            };
            files.extend(changed);
            known.insert(p.clone(), now);
        }
        files
    }

    fn unsupported(&self, fls: &Fls, log: &str) -> Vec<(&'static str, String)> {
        let mut out: Vec<_> = fls
            .outputs
            .iter()
            .filter_map(|p| match p.extension()?.to_str()? {
                "idx" => Some(("makeindex", p.display().to_string())),
                "glo" | "acn" => Some(("glossaries", p.display().to_string())),
                _ => None,
            })
            .collect();
        out.extend(scan::shell_escape(log).map(|l| ("shell-escape", l.to_string())));
        out
    }

    /// `\bibdata` databases BibTeX cannot find, when the sources ship a ready `.bbl` next to the
    /// main file (arXiv bundles do). LaTeX then reads that `.bbl`; running BibTeX would only fail.
    fn provided_bbl(&self) -> Vec<String> {
        if !self.shipped_bbl_path().is_file() {
            return Vec::new();
        }
        let lines = self.aux().1;
        let dbs = lines.lines().filter_map(|l| l.strip_prefix("\\bibdata{")?.strip_suffix('}')).flat_map(|d| d.split(','));
        let named = |d: &str| if Path::new(d).extension().is_some() { d.to_string() } else { format!("{d}.bib") };
        let names: Vec<String> = dbs.map(|d| named(d.trim())).collect();
        let found = self.resolve_all(&names);
        names.into_iter().zip(found).filter(|(_, p)| p.is_none()).map(|(n, _)| n).collect()
    }

    fn bib_tool(&self, fls: &Fls) -> Option<&'static str> {
        if fls.outputs.contains(&self.path("bcf")) {
            return Some("biber");
        }
        self.aux().1.contains("\\bibdata{").then_some("bibtex")
    }

    /// What a bibliography run depends on: its control file and the databases it last read.
    fn bib_inputs(&self, tool: &str, reads: &[(PathBuf, &str)]) -> BTreeMap<String, Option<String>> {
        let control = if tool == "biber" {
            (self.path("bcf"), self.hash(&self.path("bcf")))
        } else {
            (self.path("aux"), Some(fnv(self.aux().1.as_bytes())))
        };
        std::iter::once(control)
            .chain(reads.iter().map(|(p, _)| (p.clone(), self.hash(p))))
            .map(|(p, h)| (p.display().to_string(), h))
            .collect()
    }

    /// Where BibTeX finds each of `names`: the source or output directory first, then one
    /// kpsewhich call for the rest (it prints only what it finds, so answers match by file name).
    fn resolve_all(&self, names: &[String]) -> Vec<Option<PathBuf>> {
        let local = |n: &String| [&self.dir, &self.out].iter().map(|d| d.join(n)).find(|p| p.is_file()).and_then(|p| fs::canonicalize(p).ok());
        let mut found: Vec<Option<PathBuf>> = names.iter().map(local).collect();
        let rest: Vec<&String> = names.iter().zip(&found).filter(|(_, f)| f.is_none()).map(|(n, _)| n).collect();
        if rest.is_empty() {
            return found;
        }
        let mut c = self.cmd("kpsewhich");
        self.bibtex_env(&mut c);
        let out = c.args(&rest).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        for (n, f) in names.iter().zip(found.iter_mut()).filter(|(_, f)| f.is_none()) {
            *f = out.lines().map(PathBuf::from).find(|p| p.file_name().is_some_and(|f| f == Path::new(n).file_name().unwrap_or_default()));
        }
        found
    }

    fn report(self, ok: bool, mut inputs: Vec<(PathBuf, Option<String>)>, verified: SystemTime) -> Report {
        if !inputs.iter().any(|(p, _)| *p == self.main) {
            inputs.push((self.main.clone(), self.hash(&self.main)));
        }
        Report { ok, inputs, verified, awaited: self.awaited }
    }

    fn run(mut self) -> Report {
        let made = fs::create_dir_all(&self.out).and_then(|_| fs::canonicalize(&self.out));
        let made_err = made.as_ref().err().map(|e| e.to_string());
        if let Ok(out) = made {
            self.out = out;
        }
        let started = SystemTime::now();
        let root = self.tex_root();
        self.emit(
            "build-started",
            json!({"main": self.main, "engine": self.o.engine, "outdir": self.out, "tex_root": root}),
        );
        if let Some(e) = made_err {
            let message = format!("cannot create the output directory {}: {e}", self.out.display());
            self.emit("diagnostic", json!({"level": "error", "message": message, "raw": []}));
            self.emit("build-finished", json!({"outcome": "failed", "success": false, "steps": 0, "preview": null}));
            return self.report(false, Vec::new(), started);
        }
        let Some(root) = root else {
            let raw = vec![format!("{} not found on PATH", self.o.engine)];
            let m = scan::Missing { kind: scan::Kind::Program, name: self.o.engine.clone(), fatal: true, substitute: None, raw };
            self.emit_missing(0, &[m]);
            self.emit("build-finished", json!({"outcome": "missing", "success": false, "steps": 0, "preview": null}));
            return self.report(false, Vec::new(), started);
        };

        // Builds sharing an output namespace (directory + jobname) would overwrite each other's .aux.
        let json = self.o.json;
        let lock = acquire(&self.lock_path(), self.o.lock_timeout, |holder| {
            emit(json, "lock-wait", json!({"lock": self.lock_path(), "holder_pid": holder}));
        });
        let Ok(_lock) = lock else {
            self.emit("build-finished", json!({"outcome": "busy", "success": false, "steps": 0, "preview": null}));
            return self.report(false, Vec::new(), started);
        };

        let artifacts = [("pdf", self.path("pdf")), ("xdv", self.path("xdv")), ("synctex", self.path("synctex.gz"))];
        let state_path = self.path("texweft.json");
        let state = read_state(&state_path);
        // The last converged build's inputs: path, recorded hash, and how it was found.
        let prev: Vec<(PathBuf, Option<String>, String)> = state
            .as_ref()
            .and_then(|s| s["inputs"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|i| Some((PathBuf::from(i["path"].as_str()?), i["hash"].as_str().map(String::from), i["via"].as_str()?.to_string())))
            .collect();
        if let Some(s) = &state {
            *self.fp.borrow_mut() = Fingerprints::load(&s["fp"]);
        }
        // Every input known so far with its hash when last verified: hashed once here, then
        // refreshed by each step's verification, so no input is hashed twice per step.
        let mut known = self.hashes(prev.iter().map(|(p, ..)| p));
        let Some(mut rerun) = self.initial_rerun(state.as_ref(), &root, &prev, &known) else {
            // Up to date: nothing runs; the outputs are those of the build the state records, and
            // so is what that build reported.
            let mut s = state.unwrap_or_default();
            for m in s["reported"].as_array().into_iter().flatten() {
                let mut m = m.clone();
                let ty = m["type"].as_str().unwrap_or_default().to_string();
                if let Some(o) = m.as_object_mut() {
                    o.remove("type");
                    o.insert("step".into(), Value::Null);
                }
                emit(self.o.json, &ty, m);
            }
            for (kind, p) in artifacts.iter().filter(|(_, p)| p.exists()) {
                self.emit("artifact", json!({"kind": kind, "path": p, "fresh": false}));
            }
            let inputs: Vec<Value> = prev.iter().map(|(p, _, via)| json!({"path": p, "via": via})).collect();
            self.emit("dep-info", json!({"inputs": inputs, "programs": s["programs"]}));
            let ok = s["outcome"] == "converged";
            let preview = (s["preview"] == true).then(|| self.path("pdf"));
            self.emit("build-finished", json!({"outcome": s["outcome"], "success": ok, "steps": 0, "preview": preview}));
            // Stats that were too recent to trust when the state was written may be by now.
            if self.fp.borrow().dirty {
                let pdf = self.path("pdf");
                s["fp"] = self.fp.borrow().save(prev.iter().map(|(p, ..)| p.as_path()).chain([pdf.as_path()]));
                let _ = fs::write(&state_path, s.to_string());
            }
            return self.report(ok, known.into_iter().collect(), started);
        };
        // A build that stops midway must not leave a state that looks converged.
        let _ = fs::remove_file(&state_path);
        let start: Vec<_> = artifacts.iter().map(|(_, p)| stamp(p)).collect();
        let engine = self.o.engine.clone();
        let xdv = self.path("xdv");
        let xdv_was = if engine == "xelatex" { self.hash(&xdv) } else { None };

        let mut bib: BTreeMap<String, Option<String>> =
            state.as_ref().and_then(|s| serde_json::from_value(s["bib"].clone()).ok()).unwrap_or_default();
        let mut bib_reads: Vec<(PathBuf, &str)> = prev
            .iter()
            .filter_map(|(p, _, via)| Some((p.clone(), ["bibtex", "biber"].into_iter().find(|v| via == v)?)))
            .collect();

        let outdir_arg = format!("-output-directory={}", self.out.display());
        let file = self.main.file_name().unwrap().to_string_lossy().into_owned();
        let mut args = vec!["-interaction=nonstopmode", "-file-line-error", "-recorder", "-synctex=1", &outdir_arg];
        if engine == "xelatex" {
            args.push("-no-pdf");
        }
        args.push(&file);

        let (mut runs, mut engine_step, mut created) = (0, 0, 0);
        self.shipped_bbl = !self.provided_bbl().is_empty();
        // As latexmk does: start from the .aux a one-page document without cross-references
        // writes. LaTeX reads it at \begin{document}, so when the first run writes the same, the
        // run already read its own result and no second run is needed.
        let aux = self.path("aux");
        if !aux.exists() {
            let _ = fs::write(&aux, "\\relax \n\\gdef \\@abspage@last{1}\n");
        }
        let mut fls = Fls::parse(&read(&self.path("fls")));
        for p in self.engine_inputs(&fls) {
            known.entry(p).or_insert_with_key(|p| self.hash(p));
        }
        let mut log = String::new();
        // The last engine run's findings, reported after the loop: a rerun clears earlier warnings.
        let (mut diags, mut missing, mut unsupported) = (Vec::new(), Vec::new(), Vec::new());
        // Whether the latest engine run reported an error; the build is `failed` if the last one did.
        let mut errored = false;
        let mut used_provided_bbl = false;
        // When the last verification of the engine's inputs began. An input modified before it was
        // either verified unchanged by it or caused a rerun; one modified after it is unverified.
        let mut checked_at = started;
        let (outcome, last_rerun) = loop {
            if runs == MAX_ENGINE_RUNS {
                break (if errored { "failed" } else { "not-converged" }, Some(rerun));
            }
            let before = self.hashes(fls.outputs.iter().filter(|p| self.may_feed_back(p)).chain([&aux]));
            for d in self.include_dirs().into_iter().take(MAX_CREATED_DIRS - created) {
                created += fs::create_dir_all(d).is_ok() as usize;
            }
            // An engine that dies before writing them must not leave the last run's log and fls to be read.
            let _ = fs::remove_file(self.path("log"));
            let _ = fs::remove_file(self.path("fls"));
            runs += 1;
            let run_started = SystemTime::now();
            let mut cmd = self.cmd(&engine);
            cmd.args(&args);
            let ran = self.start_step(&engine, cmd, rerun);
            engine_step = self.step;
            (fls, log) = (Fls::parse(&read(&self.path("fls"))), read(&self.path("log")));
            let tool_missing = self.tool_missing(&engine, &ran);
            checked_at = SystemTime::now();
            let mut files = self.changed_during(&self.engine_inputs(&fls), &mut known, run_started);

            // `\include{ch/one}` stops the first run: TeX cannot create `ch/` in the output
            // directory. Recover only when the unwritable file is an include target the .aux names,
            // and only up to MAX_CREATED_DIRS; this preparation is not an engine run of the build.
            let dirs: Vec<PathBuf> = self.include_dirs().into_iter().take(MAX_CREATED_DIRS - created).collect();
            if ran.code != Some(0)
                && scan::unwritable(&log).is_some_and(|f| dirs.iter().any(|d| self.out.join(f).parent() == Some(d.as_path())))
            {
                let dirs: Vec<PathBuf> = dirs.into_iter().filter(|d| fs::create_dir_all(d).is_ok()).collect();
                created += dirs.len();
                self.finish_step(&engine, &ran, &self.path("log"));
                runs -= 1;
                rerun = json!({"kind": "output-dirs-created", "dirs": dirs});
                continue;
            }

            diags = scan::diagnostics(&log);
            missing = scan::missing(&log);
            unsupported = self.unsupported(&fls, &log);
            let has_error = diags.iter().any(|d| d.level == "error");
            if (ran.timed_out || ran.code != Some(0) && !has_error) && !tool_missing {
                self.tool_failed(&engine, &ran);
            }
            self.finish_step(&engine, &ran, &self.path("log"));
            if tool_missing || missing.iter().any(|m| m.fatal) {
                break ("missing", None);
            }
            if !unsupported.is_empty() {
                break ("unsupported", None);
            }
            // An error does not stop the build when the run still left what the next steps need: the
            // .aux it wrote (BibTeX and the next run read it) and no abort, after which TeX's files
            // are truncated. The outcome stays `failed` while the last run errs; continuing only
            // completes citations and references in the preview, as latexmk -f and Overleaf do.
            errored = ran.code != Some(0) || has_error;
            let aborted = ran.timed_out || scan::aborted(&log) || !fls.outputs.contains(&self.path("aux"));
            if errored && aborted {
                break ("failed", None);
            }

            files.extend(self.feedback(&fls, &before));
            let mut tool = self.bib_tool(&fls);
            if tool == Some("bibtex") {
                let unfound = self.provided_bbl();
                self.shipped_bbl = !unfound.is_empty();
                if self.shipped_bbl {
                    tool = None;
                    // Nothing generates the .bbl any more: forget BibTeX's state, so clean keeps the
                    // shipped file, and drop a .bbl BibTeX left in the output directory, which
                    // LaTeX would find before the shipped one.
                    bib.clear();
                    bib_reads.clear();
                    let made = self.path("bbl");
                    if made != self.shipped_bbl_path() && made.exists() {
                        let was = self.hash(&made);
                        if fs::remove_file(&made).is_ok() {
                            files.push(json!({"path": made, "was": was, "now": null}));
                        }
                    }
                    if !used_provided_bbl {
                        used_provided_bbl = true;
                        let bbl = self.shipped_bbl_path();
                        let m: Vec<scan::Missing> = unfound
                            .into_iter()
                            .map(|name| scan::Missing {
                                kind: scan::Kind::File,
                                raw: vec![format!("{name} not found; using {}", bbl.display())],
                                name,
                                fatal: false,
                                substitute: Some("bbl"),
                            })
                            .collect();
                        self.emit_missing(self.step, &m);
                    }
                }
            }
            if let Some(tool) = tool {
                let now = self.bib_inputs(tool, &bib_reads);
                let reason = if bib.is_empty() || !self.path("bbl").exists() {
                    Some(json!({"kind": "no-previous-run"}))
                } else {
                    let keys: std::collections::BTreeSet<_> = bib.keys().chain(now.keys()).collect();
                    changed(keys.into_iter()
                        .filter(|k| bib.get(*k) != now.get(*k))
                        .map(|k| json!({"path": k, "was": bib.get(k).cloned().flatten(), "now": now.get(k).cloned().flatten()}))
                        .collect())
                };
                if let Some(reason) = reason {
                    let bbl = self.path("bbl");
                    let bbl_was = self.hash(&bbl);
                    let _ = fs::remove_file(self.path("blg"));
                    let bib_started = SystemTime::now();
                    let mut bib_known = self.hashes(bib_reads.iter().map(|(p, _)| p));
                    let mut cmd = self.cmd(tool);
                    if tool == "bibtex" {
                        self.bibtex_env(&mut cmd);
                        cmd.current_dir(&self.out).arg(&self.job);
                    } else {
                        cmd.arg("--output-directory").arg(&self.out).arg(&self.job);
                    }
                    let ran = self.start_step(tool, cmd, reason);
                    let tool_missing = self.tool_missing(tool, &ran);
                    let blg = read(&self.path("blg"));
                    let (reads, blg_missing) = scan::blg(&blg);
                    self.emit_missing(self.step, &blg_missing);
                    // BibTeX exits 1 for warnings only.
                    let failed = !tool_missing && ran.code.is_none_or(|c| c > (tool == "bibtex") as i32);
                    if failed {
                        self.tool_failed(tool, &ran);
                    }
                    self.finish_step(tool, &ran, &self.path("blg"));
                    let reads: Vec<String> = reads.into_iter().map(String::from).collect();
                    bib_reads = self.resolve_all(&reads).into_iter().flatten().map(|p| (p, tool)).collect();
                    bib = self.bib_inputs(tool, &bib_reads);
                    // A database saved while the tool ran: record no hash, so the tool runs again.
                    let read: Vec<PathBuf> = bib_reads.iter().map(|(p, _)| p.clone()).collect();
                    for f in self.changed_during(&read, &mut bib_known, bib_started) {
                        if let Some(h) = f["path"].as_str().and_then(|k| bib.get_mut(k)) {
                            *h = None;
                        }
                        files.push(f);
                    }
                    if tool_missing || blg_missing.iter().any(|m| m.fatal) {
                        break ("missing", None);
                    }
                    if failed {
                        break ("failed", None);
                    }
                    let bbl_now = self.hash(&bbl);
                    if bbl_now != bbl_was {
                        files.push(json!({"path": bbl, "was": bbl_was, "now": bbl_now}));
                    }
                }
            }
            match changed(files).or_else(|| scan::rerun_request(&log).map(|m| json!({"kind": "log-requested", "message": m}))) {
                Some(r) => rerun = r,
                None => break (if errored { "failed" } else { "converged" }, None),
            }
        };

        for d in diags {
            let mut msg = json!({"step": engine_step, "level": d.level});
            if let Some(c) = d.code {
                msg["code"] = c.into();
            }
            msg["message"] = json!(d.message);
            if let Some((f, line)) = &d.span {
                let f = f.as_ref().map(|f| fs::canonicalize(self.dir.join(f)).unwrap_or_else(|_| self.dir.join(f)));
                msg["span"] = json!({"file": f, "line": line});
            }
            msg["raw"] = json!(d.raw);
            self.emit("diagnostic", msg);
        }
        self.emit_missing(engine_step, &missing);
        for (flow, evidence) in unsupported {
            self.emit("unsupported", json!({"step": engine_step, "flow": flow, "evidence": evidence}));
        }

        let mut outcome = outcome;
        let mut pdf_step = engine_step;
        let xdv_bytes = if engine == "xelatex" { fs::read(&xdv).unwrap_or_default() } else { Vec::new() };
        if engine == "xelatex" && stamp(&xdv) != start[1] && outcome != "missing" && outcome != "unsupported" {
            let now = (!xdv_bytes.is_empty()).then(|| fnv(&xdv_bytes));
            let reason = json!({"kind": "inputs-changed", "files": [{"path": xdv, "was": xdv_was, "now": now}]});
            // From the source directory: images in the .xdv are named relative to it.
            let mut cmd = self.cmd("xdvipdfmx");
            cmd.arg("-q").arg("-o").arg(self.path("pdf")).arg(&xdv);
            let ran = self.start_step("xdvipdfmx", cmd, reason);
            pdf_step = self.step;
            if self.tool_missing("xdvipdfmx", &ran) {
                if outcome == "converged" {
                    outcome = "missing";
                }
            } else if ran.code != Some(0) {
                self.tool_failed("xdvipdfmx", &ran);
                if outcome == "converged" {
                    outcome = "failed";
                }
            }
            self.finish_step("xdvipdfmx", &ran, Path::new(""));
        }

        let mut preview = None;
        for ((kind, p), was) in artifacts.iter().zip(&start) {
            if p.exists() {
                let fresh = stamp(p) != *was;
                let step = if *kind == "pdf" { pdf_step } else { engine_step };
                self.emit("artifact", json!({"kind": kind, "path": p, "step": fresh.then_some(step), "fresh": fresh}));
                if *kind == "pdf" && fresh {
                    preview = Some(p.clone());
                }
            }
        }

        let paths = self.engine_inputs(&fls).into_iter();
        let mut deps: Vec<(PathBuf, &str)> = if engine == "lualatex" {
            scan::lua_inputs(paths, &log)
        } else {
            paths.map(|p| (p, "fls")).collect()
        };
        deps.extend(scan::xdv_fonts(&xdv_bytes).into_iter().map(|p| (p.into(), "xdv")));
        deps.extend(bib_reads.iter().cloned());
        let mut seen = std::collections::BTreeSet::new();
        deps.retain(|(p, _)| seen.insert(p.clone()));
        let inputs: Vec<Value> = deps.iter().map(|(p, via)| json!({"path": p, "via": via})).collect();
        // The programs this document's build needs, whether or not this build had to run them: an
        // incremental build that skips BibTeX still needs it (a lockfile must keep its package).
        let mut programs = vec![engine.clone()];
        programs.extend(self.bib_tool(&fls).filter(|_| !self.shipped_bbl).map(String::from));
        if engine == "xelatex" {
            programs.push("xdvipdfmx".into());
        }
        self.emit("dep-info", json!({"inputs": inputs, "programs": programs}));

        // An input modified after the last verification may not be what the engine read: record no
        // hash, so the next build reruns. mtimes past the build's end (clock skew, unpacked archives)
        // are not edits, or such a file would never settle. Settled inputs keep the hash their
        // verification took. ponytail: assumes sub-second mtimes (ext4, APFS, btrfs).
        let finished = SystemTime::now();
        let settled = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t < checked_at || t > finished);
        let seen: Vec<(PathBuf, Option<String>)> = deps
            .iter()
            .map(|(p, _)| (p.clone(), settled(p).then(|| known.get(p).cloned().unwrap_or_else(|| self.hash(p))).flatten()))
            .collect();
        // A failure the document itself causes is as reproducible as success, so it is reused as
        // latexmk does: the same inputs fail the same way. Not one with tool trouble (a crash, a
        // timeout, a tool's own error) or one that hit the run limit, which another try may get past.
        let reusable = outcome == "converged" || outcome == "failed" && !self.tool_trouble.get() && last_rerun.is_none();
        if reusable {
            let inputs: Vec<Value> =
                deps.iter().zip(&seen).map(|((p, via), (_, h))| json!({"path": p, "via": via, "hash": h})).collect();
            for (k, v) in bib.iter_mut() {
                if bib_reads.iter().any(|(p, _)| p == Path::new(k)) && !settled(Path::new(k)) {
                    *v = None;
                }
            }
            let pdf = self.path("pdf");
            let pdf_hash = self.hash(&pdf);
            let fp = self.fp.borrow().save(deps.iter().map(|(p, _)| p.as_path()).chain([pdf.as_path()]));
            let state = json!({"v": STATE_VERSION, "tex_root": root, "engine": engine, "pdf": pdf_hash,
                "inputs": inputs, "programs": programs, "bib": bib, "outcome": outcome, "preview": preview.is_some(),
                "reported": *self.reported.borrow(), "fp": fp});
            let _ = fs::write(&state_path, state.to_string());
        }
        let mut done = json!({"outcome": outcome, "success": outcome == "converged", "steps": self.step, "preview": preview});
        if let Some(r) = last_rerun {
            done["last_rerun"] = r;
        }
        self.emit("build-finished", done);
        self.report(outcome == "converged", seen, checked_at)
    }
}

/// Removes the outputs texweft manages for `main`: the recorded outputs inside the output
/// directory, the known artifacts, and directories that held only those. The output directory and
/// the lock file stay: deleting the lock would let a waiting texweft lock an unlinked file.
fn clean(main: &Path, o: &Opts) -> bool {
    let mut b = Build::new(main, o, false);
    let Ok(out) = fs::canonicalize(&b.out) else { return true };
    b.out = out.clone();
    let lock = b.lock_path();
    let waiting = |holder: Option<u32>| eprintln!("texweft: waiting for the build holding {} (pid {holder:?})", lock.display());
    let Ok(_lock) = acquire(&lock, o.lock_timeout, waiting) else {
        eprintln!("texweft: another build is still using these outputs; nothing removed");
        return false;
    };
    // Only files whose directory, with symlinks resolved, is inside the output directory:
    // `out/../x` and `out/link/x` (link → elsewhere) start with `out` but name files outside it.
    // The file itself may be a symlink; removing it removes the link, not its target.
    let inside = |p: &Path| {
        !p.components().any(|c| c == std::path::Component::ParentDir)
            && p.parent().and_then(|d| fs::canonicalize(d).ok()).is_some_and(|d| d.starts_with(&out))
    };
    let mut files: std::collections::BTreeSet<PathBuf> =
        Fls::parse(&read(&b.path("fls"))).outputs.into_iter().filter(|p| inside(p)).collect();
    let fixed = ["fls", "log", "aux", "blg", "bcf", "run.xml", "pdf", "xdv", "synctex.gz"];
    files.extend(fixed.map(|e| b.path(e)));
    // A .bbl may be the only copy (arXiv sources ship one without the .bib). Remove it only when
    // the state shows texweft generated it with BibTeX or Biber.
    if read_state(&b.path("texweft.json")).is_some_and(|s| s["bib"].as_object().is_some_and(|b| !b.is_empty())) {
        files.insert(b.path("bbl"));
    } else {
        files.remove(&b.path("bbl"));
    }
    files.insert(b.path("texweft.json"));
    files.remove(main);
    let mut dirs = std::collections::BTreeSet::new();
    for f in files {
        if fs::remove_file(&f).is_ok() {
            eprintln!("removed {}", f.display());
            dirs.extend(f.ancestors().skip(1).take_while(|d| *d != out).map(Path::to_path_buf));
        }
    }
    // Deepest first; remove_dir refuses anything not empty, so user files are never touched.
    for d in dirs.iter().rev() {
        if fs::remove_dir(d).is_ok() {
            eprintln!("removed {}/", d.display());
        }
    }
    true
}

fn human(m: &Value) {
    let s = |k: &str| m[k].as_str().unwrap_or("").to_string();
    match s("type").as_str() {
        "step-started" => eprintln!("{:>10} {} ({})", "Running", s("tool"), m["rerun"]["kind"].as_str().unwrap_or("")),
        "diagnostic" => {
            eprintln!("{}: {}", s("level"), s("message"));
            if let Some(line) = m["span"]["line"].as_u64() {
                eprintln!("  --> {}:{line}", m["span"]["file"].as_str().unwrap_or("?"));
            }
        }
        "missing" => eprintln!("missing {}: {}{}", s("kind"), s("name"), if m["fatal"] == true { "" } else { " (substituted)" }),
        "unsupported" => eprintln!("unsupported {}: {}", s("flow"), s("evidence")),
        "lock-wait" => eprintln!("{:>10} for the build holding {} (pid {})", "Waiting", s("lock"), m["holder_pid"]),
        "watching" => eprintln!("{:>10} {} inputs ({}), Ctrl-C to stop", "Watching", m["inputs"], s("backend")),
        "build-finished" => {
            let preview = m["preview"].as_str().map(|p| format!(", preview {p}")).unwrap_or_default();
            eprintln!("{:>10} {}, {} step(s){preview}", "Finished", s("outcome"), m["steps"]);
        }
        _ => {}
    }
}
