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

/// Engine runs per invocation, counting the first (latexmk's `$max_repeat`).
const MAX_ENGINE_RUNS: u32 = 5;
const USAGE: &str = "usage: texweft build <main.tex> [options]
       texweft watch <main.tex> [options] [--poll]
       texweft clean <main.tex> [--outdir DIR] [--lock-timeout SECS]
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
    pub tex_root: Option<PathBuf>,
    /// Inputs and the content hash the build used; `None` when unknown (saved mid-build).
    pub inputs: Vec<(PathBuf, Option<String>)>,
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
    file: String,
    job: String,
    engine: String,
    bin: Option<PathBuf>,
    json: bool,
    force: bool,
    step_timeout: Option<Duration>,
    lock_timeout: Option<Duration>,
    step: u32,
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

fn stamp(p: &Path) -> Option<(SystemTime, String)> {
    Some((fs::metadata(p).ok()?.modified().ok()?, hash(p)?))
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

/// The `\@input{...}` targets (include `.aux` files) of an `.aux`, recursively, as written.
fn aux_inputs(aux: &Path, out: &mut Vec<String>) {
    for l in read(aux).lines() {
        if let Some(sub) = l.strip_prefix("\\@input{").and_then(|r| r.strip_suffix('}'))
            && !out.iter().any(|o| o == sub)
        {
            out.push(sub.to_string());
            aux_inputs(&aux.with_file_name(sub), out);
        }
    }
}

/// `\citation`, `\bibdata`, `\bibstyle` lines of an `.aux` and the `.aux` files it `\@input`s.
fn aux_bib_lines(aux: &Path, out: &mut String) {
    for l in read(aux).lines() {
        if let Some(sub) = l.strip_prefix("\\@input{").and_then(|r| r.strip_suffix('}')) {
            aux_bib_lines(&aux.with_file_name(sub), out);
        } else if ["\\citation", "\\bibdata", "\\bibstyle"].iter().any(|p| l.starts_with(p)) {
            out.push_str(l);
            out.push('\n');
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
        thread::sleep(Duration::from_millis(5));
    };
    let text = |t: thread::JoinHandle<Vec<u8>>| String::from_utf8_lossy(&t.join().unwrap_or_default()).into_owned();
    Ok((code, text(out) + &text(err), timed_out))
}

impl Build {
    fn new(main: &Path, o: &Opts, force: bool) -> Build {
        Build {
            main: main.to_path_buf(),
            awaited: Vec::new(),
            dir: main.parent().unwrap().to_path_buf(),
            out: main.parent().unwrap().join(&o.outdir),
            file: main.file_name().unwrap().to_string_lossy().into(),
            job: main.file_stem().unwrap().to_string_lossy().into(),
            bin: find_bin(&o.engine),
            engine: o.engine.clone(),
            json: o.json,
            force,
            step_timeout: o.step_timeout,
            lock_timeout: o.lock_timeout,
            step: 0,
        }
    }

    fn path(&self, ext: &str) -> PathBuf {
        self.out.join(format!("{}.{ext}", self.job))
    }

    fn lock_path(&self) -> PathBuf {
        lock_path(&self.out, &self.job)
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
        let sep = if cfg!(windows) { ";" } else { ":" };
        for var in ["BIBINPUTS", "BSTINPUTS"] {
            let rest = env::var(var).unwrap_or_default();
            c.env(var, format!("{}{sep}{rest}", self.dir.display()));
        }
    }

    /// Output directories for `\include` targets the `.aux` tree names: TeX cannot create them.
    /// Only plain relative paths inside the output directory, so a document cannot steer this.
    fn include_dirs(&self) -> Vec<PathBuf> {
        let mut targets = Vec::new();
        aux_inputs(&self.path("aux"), &mut targets);
        targets
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
        emit(self.json, ty, fields);
    }

    fn tex_root(&self) -> Option<PathBuf> {
        let bin = self.bin.as_ref()?;
        let out = Command::new(bin.join("kpsewhich")).arg("-var-value=SELFAUTOPARENT").output().ok();
        let root = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
        fs::canonicalize(root.map(PathBuf::from).or_else(|| Some(bin.parent()?.parent()?.to_path_buf()))?).ok()
    }

    fn start_step(&mut self, tool: &str, mut cmd: Command, rerun: Value) -> Ran {
        self.step += 1;
        self.emit("step-started", json!({"step": self.step, "tool": tool, "rerun": rerun}));
        let t = Instant::now();
        let out = run_tool(&mut cmd, self.step_timeout);
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

    fn initial_rerun(&self, state: Option<&Value>, root: &Path) -> Option<Value> {
        if self.force {
            return Some(json!({"kind": "forced"}));
        }
        let no_prev = Some(json!({"kind": "no-previous-run"}));
        let Some(s) = state else { return no_prev };
        if s["tex_root"] != json!(root) {
            return Some(json!({"kind": "toolchain-changed", "was": s["tex_root"], "now": root}));
        }
        if s["engine"] != self.engine.as_str() {
            return Some(json!({"kind": "toolchain-changed", "was": s["engine"], "now": self.engine}));
        }
        if s["pdf"].as_str() != hash(&self.path("pdf")).as_deref() {
            return no_prev;
        }
        let files = s["inputs"].as_array().into_iter().flatten().filter_map(|i| {
            let p = Path::new(i["path"].as_str()?);
            let now = hash(p);
            (i["hash"].as_str() != now.as_deref()).then(|| json!({"path": p, "was": i["hash"], "now": now}))
        });
        changed(files.collect())
    }

    /// Outputs the next engine run reads back and that this run changed.
    fn feedback(&self, fls: &str, before: &BTreeMap<PathBuf, Option<String>>) -> Vec<Value> {
        let inputs = scan::fls_paths(fls, "INPUT");
        let mut files = Vec::new();
        for p in scan::fls_paths(fls, "OUTPUT") {
            let name = p.to_string_lossy();
            // Caches the engine writes elsewhere (luaotfload under texmf-var) are not document state.
            if !p.starts_with(&self.out) || NOT_FEEDBACK.iter().any(|e| name.ends_with(e) || (*e == "synctex" && name.contains(e))) {
                continue;
            }
            let (was, now) = (before.get(&p).cloned().flatten(), hash(&p));
            // Read this run: did it change? Not read: only a newly created file may be read next run.
            let rerun = if inputs.contains(&p) { was != now } else { was.is_none() && now.is_some() };
            if rerun {
                files.push(json!({"path": p, "was": was, "now": now}));
            }
        }
        files
    }

    /// What one engine run read: `.fls` inputs that no step of the build writes.
    fn engine_inputs(&self, fls: &str) -> Vec<PathBuf> {
        let outputs = scan::fls_paths(fls, "OUTPUT");
        let bbl = self.path("bbl");
        scan::fls_paths(fls, "INPUT").into_iter().filter(|p| !outputs.contains(p) && *p != bbl).collect()
    }

    /// Inputs that may have changed while a step ran, so its result may not match them: a known
    /// input whose hash differs from the one taken just before the step, or an input first seen in
    /// this step whose mtime falls inside it. Hashes before and after cannot prove the tool read one
    /// consistent version, but any change they reveal is enough to distrust the result.
    fn changed_during(&self, read: &[PathBuf], before: &BTreeMap<PathBuf, Option<String>>, from: SystemTime) -> Vec<Value> {
        let to = SystemTime::now();
        read.iter()
            .filter_map(|p| {
                let now = hash(p);
                match before.get(p) {
                    Some(was) => (*was != now).then(|| json!({"path": p, "was": was, "now": now})),
                    None => fs::metadata(p)
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t >= from && t <= to)
                        .then(|| json!({"path": p, "was": null, "now": now})),
                }
            })
            .collect()
    }

    fn unsupported(&self, fls: &str, log: &str) -> Vec<(&'static str, String)> {
        let mut out: Vec<_> = scan::fls_paths(fls, "OUTPUT")
            .into_iter()
            .filter_map(|p| match p.extension()?.to_str()? {
                "idx" => Some(("makeindex", p.display().to_string())),
                "glo" | "acn" => Some(("glossaries", p.display().to_string())),
                _ => None,
            })
            .collect();
        out.extend(log.lines().find(|l| l.contains("runsystem(") && !l.ends_with("disabled.")).map(|l| ("shell-escape", l.to_string())));
        out
    }

    /// `\bibdata` databases BibTeX cannot find, when the sources ship a ready `.bbl` next to the
    /// main file (arXiv bundles do). LaTeX then reads that `.bbl`; running BibTeX would only fail.
    fn provided_bbl(&self) -> Vec<String> {
        if !self.dir.join(format!("{}.bbl", self.job)).is_file() {
            return Vec::new();
        }
        let mut lines = String::new();
        aux_bib_lines(&self.path("aux"), &mut lines);
        let dbs = lines.lines().filter_map(|l| l.strip_prefix("\\bibdata{")?.strip_suffix('}')).flat_map(|d| d.split(','));
        let named = |d: &str| if Path::new(d).extension().is_some() { d.to_string() } else { format!("{d}.bib") };
        dbs.map(|d| named(d.trim())).filter(|d| self.resolve(d).is_none()).collect()
    }

    fn bib_tool(&self, fls: &str) -> Option<&'static str> {
        if scan::fls_paths(fls, "OUTPUT").contains(&self.path("bcf")) {
            return Some("biber");
        }
        let mut lines = String::new();
        aux_bib_lines(&self.path("aux"), &mut lines);
        lines.contains("\\bibdata{").then_some("bibtex")
    }

    /// What a bibliography run depends on: its control file and the databases it last read.
    fn bib_inputs(&self, tool: &str, reads: &[(PathBuf, &str)]) -> BTreeMap<String, Option<String>> {
        let control = if tool == "biber" {
            (self.path("bcf"), hash(&self.path("bcf")))
        } else {
            let mut lines = String::new();
            aux_bib_lines(&self.path("aux"), &mut lines);
            (self.path("aux"), Some(fnv(lines.as_bytes())))
        };
        std::iter::once(control)
            .chain(reads.iter().map(|(p, _)| (p.clone(), hash(p))))
            .map(|(p, h)| (p.display().to_string(), h))
            .collect()
    }

    fn resolve(&self, name: &str) -> Option<PathBuf> {
        if let Some(local) = [&self.dir, &self.out].iter().map(|d| d.join(name)).find(|p| p.is_file()) {
            return fs::canonicalize(local).ok();
        }
        let mut c = self.cmd("kpsewhich");
        self.bibtex_env(&mut c);
        let o = c.arg(name).output().ok()?;
        let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (!p.is_empty()).then(|| PathBuf::from(p))
    }

    fn report(self, ok: bool, tex_root: Option<PathBuf>, mut inputs: Vec<(PathBuf, Option<String>)>) -> Report {
        if !inputs.iter().any(|(p, _)| *p == self.main) {
            inputs.push((self.main.clone(), hash(&self.main)));
        }
        Report { ok, tex_root, inputs, awaited: self.awaited }
    }

    fn run(mut self) -> Report {
        let made = fs::create_dir_all(&self.out).and_then(|_| fs::canonicalize(&self.out));
        let made_err = made.as_ref().err().map(|e| e.to_string());
        if let Ok(out) = made {
            self.out = out;
        }
        let artifacts = [("pdf", self.path("pdf")), ("xdv", self.path("xdv")), ("synctex", self.path("synctex.gz"))];
        let start: Vec<_> = artifacts.iter().map(|(_, p)| stamp(p)).collect();
        let started = SystemTime::now();
        let root = self.tex_root();
        self.emit(
            "build-started",
            json!({"main": self.dir.join(&self.file), "engine": self.engine, "outdir": self.out, "tex_root": root}),
        );
        if let Some(e) = made_err {
            let message = format!("cannot create the output directory {}: {e}", self.out.display());
            self.emit("diagnostic", json!({"level": "error", "message": message, "raw": []}));
            self.emit("build-finished", json!({"outcome": "failed", "success": false, "steps": 0, "preview": null}));
            return self.report(false, root, Vec::new());
        }
        let Some(root) = root else {
            let raw = vec![format!("{} not found on PATH", self.engine)];
            let m = scan::Missing { kind: scan::Kind::Program, name: self.engine.clone(), fatal: true, substitute: None, raw };
            self.emit_missing(0, &[m]);
            self.emit("build-finished", json!({"outcome": "missing", "success": false, "steps": 0, "preview": null}));
            return self.report(false, None, Vec::new());
        };

        // Builds sharing an output namespace (directory + jobname) would overwrite each other's .aux.
        let json = self.json;
        let lock = acquire(&self.lock_path(), self.lock_timeout, |holder| {
            emit(json, "lock-wait", json!({"lock": self.lock_path(), "holder_pid": holder}));
        });
        let Ok(_lock) = lock else {
            self.emit("build-finished", json!({"outcome": "busy", "success": false, "steps": 0, "preview": null}));
            return self.report(false, Some(root), Vec::new());
        };

        let state_path = self.path("texweft.json");
        let state: Option<Value> = fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let Some(mut rerun) = self.initial_rerun(state.as_ref(), &root) else {
            // Up to date: nothing runs; the PDF matches the last converged build.
            for (kind, p) in artifacts.iter().filter(|(_, p)| p.exists()) {
                self.emit("artifact", json!({"kind": kind, "path": p, "fresh": false}));
            }
            let inputs: Vec<Value> = state.as_ref().and_then(|s| s["inputs"].as_array()).into_iter().flatten()
                .map(|i| json!({"path": i["path"], "via": i["via"]}))
                .collect();
            self.emit("dep-info", json!({"inputs": inputs}));
            self.emit("build-finished", json!({"outcome": "converged", "success": true, "steps": 0, "preview": self.path("pdf")}));
            let seen = state.as_ref().and_then(|s| s["inputs"].as_array()).into_iter().flatten();
            let seen = seen.filter_map(|i| Some((PathBuf::from(i["path"].as_str()?), i["hash"].as_str().map(String::from))));
            return self.report(true, Some(root), seen.collect());
        };
        // A build that stops midway must not leave a state that looks converged.
        let _ = fs::remove_file(&state_path);

        let mut bib: BTreeMap<String, Option<String>> =
            state.as_ref().and_then(|s| serde_json::from_value(s["bib"].clone()).ok()).unwrap_or_default();
        let mut bib_reads: Vec<(PathBuf, &str)> = state
            .as_ref()
            .and_then(|s| s["inputs"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let via = ["bibtex", "biber"].into_iter().find(|v| i["via"] == *v)?;
                Some((PathBuf::from(i["path"].as_str()?), via))
            })
            .collect();

        let engine = self.engine.clone();
        let outdir_arg = format!("-output-directory={}", self.out.display());
        let mut args = vec!["-interaction=nonstopmode", "-file-line-error", "-recorder", "-synctex=1", &outdir_arg];
        if engine == "xelatex" {
            args.push("-no-pdf");
        }
        let file = self.file.clone();
        args.push(&file);

        let (mut runs, mut engine_step, mut created) = (0, 0, 0);
        let (mut fls, mut log) = (String::new(), String::new());
        // Every input known so far: from the last converged build, the previous run's .fls, and
        // what the bibliography tool read. Hashed right before each step to verify it afterwards.
        let mut known: std::collections::BTreeSet<PathBuf> = state
            .as_ref()
            .and_then(|s| s["inputs"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|i| i["path"].as_str().map(PathBuf::from))
            .collect();
        known.extend(self.engine_inputs(&read(&self.path("fls"))));
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
            let before: BTreeMap<_, _> = scan::fls_paths(&read(&self.path("fls")), "OUTPUT")
                .into_iter()
                .map(|p| {
                    let h = hash(&p);
                    (p, h)
                })
                .collect();
            for d in self.include_dirs().into_iter().take(MAX_CREATED_DIRS - created) {
                created += fs::create_dir_all(d).is_ok() as usize;
            }
            // An engine that dies before writing them must not leave the last run's log and fls to be read.
            let _ = fs::remove_file(self.path("log"));
            let _ = fs::remove_file(self.path("fls"));
            runs += 1;
            let known_before: BTreeMap<PathBuf, Option<String>> = known.iter().map(|p| (p.clone(), hash(p))).collect();
            let run_started = SystemTime::now();
            let mut cmd = self.cmd(&engine);
            cmd.args(&args);
            let ran = self.start_step(&engine, cmd, rerun);
            engine_step = self.step;
            (fls, log) = (read(&self.path("fls")), read(&self.path("log")));
            let tool_missing = self.tool_missing(&engine, &ran);
            let read_now = self.engine_inputs(&fls);
            checked_at = SystemTime::now();
            let mut files = self.changed_during(&read_now, &known_before, run_started);
            known.extend(read_now);

            // `\include{ch/one}` stops the first run: TeX cannot create `ch/` in the output
            // directory. Recover only when the unwritable file is an include target the .aux names,
            // and only up to MAX_CREATED_DIRS; this preparation is not an engine run of the build.
            let unwritable = log.lines().find_map(|l| scan::quoted(l, "I can't write on file `", "'"));
            let dirs: Vec<PathBuf> = self.include_dirs().into_iter().take(MAX_CREATED_DIRS - created).collect();
            if ran.code != Some(0)
                && unwritable.is_some_and(|f| dirs.iter().any(|d| self.out.join(f).parent() == Some(d.as_path())))
            {
                let dirs: Vec<PathBuf> = dirs.into_iter().filter(|d| fs::create_dir_all(d).is_ok()).collect();
                created += dirs.len();
                self.finish_step(&engine, &ran, &self.path("log"));
                runs -= 1;
                rerun = json!({"kind": "output-dirs-created", "dirs": dirs});
                continue;
            }

            // Reported after the loop, for the last engine run only: a rerun clears earlier warnings.
            let has_error = scan::diagnostics(&log).iter().any(|d| d.level == "error");
            let missing = scan::missing(&log);
            let unsupported = self.unsupported(&fls, &log);
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
            let aborted = ran.timed_out
                || log.contains("Emergency stop")
                || log.contains("Fatal error occurred")
                || !scan::fls_paths(&fls, "OUTPUT").contains(&self.path("aux"));
            if errored && aborted {
                break ("failed", None);
            }

            files.extend(self.feedback(&fls, &before));
            let mut tool = self.bib_tool(&fls);
            if tool == Some("bibtex") {
                let unfound = self.provided_bbl();
                if !unfound.is_empty() {
                    tool = None;
                    if !used_provided_bbl {
                        used_provided_bbl = true;
                        let bbl = self.dir.join(format!("{}.bbl", self.job));
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
                    let bbl_was = hash(&bbl);
                    let _ = fs::remove_file(self.path("blg"));
                    let bib_started = SystemTime::now();
                    let bib_before: BTreeMap<PathBuf, Option<String>> = bib_reads.iter().map(|(p, _)| (p.clone(), hash(p))).collect();
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
                    let (reads, missing) = scan::blg(&blg);
                    self.emit_missing(self.step, &missing);
                    // BibTeX exits 1 for warnings only.
                    let failed = !tool_missing && ran.code.is_none_or(|c| c > (tool == "bibtex") as i32);
                    if failed {
                        self.tool_failed(tool, &ran);
                    }
                    self.finish_step(tool, &ran, &self.path("blg"));
                    bib_reads = reads.iter().filter_map(|r| Some((self.resolve(r)?, tool))).collect();
                    bib = self.bib_inputs(tool, &bib_reads);
                    // A database saved while the tool ran: record no hash, so the tool runs again.
                    let read: Vec<PathBuf> = bib_reads.iter().map(|(p, _)| p.clone()).collect();
                    for f in self.changed_during(&read, &bib_before, bib_started) {
                        if let Some(h) = f["path"].as_str().and_then(|k| bib.get_mut(k)) {
                            *h = None;
                        }
                        files.push(f);
                    }
                    if tool_missing || missing.iter().any(|m| m.fatal) {
                        break ("missing", None);
                    }
                    if failed {
                        break ("failed", None);
                    }
                    let bbl_now = hash(&bbl);
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

        for d in scan::diagnostics(&log) {
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
        self.emit_missing(engine_step, &scan::missing(&log));
        for (flow, evidence) in self.unsupported(&fls, &log) {
            self.emit("unsupported", json!({"step": engine_step, "flow": flow, "evidence": evidence}));
        }

        let mut outcome = outcome;
        let mut pdf_step = engine_step;
        let xdv = self.path("xdv");
        if engine == "xelatex" && stamp(&xdv) != start[1] && outcome != "missing" && outcome != "unsupported" {
            let reason = changed(vec![json!({"path": xdv, "was": start[1].as_ref().map(|s| &s.1), "now": hash(&xdv)})]).unwrap();
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

        let outputs = scan::fls_paths(&fls, "OUTPUT");
        let mut deps: Vec<(PathBuf, &str)> = scan::fls_paths(&fls, "INPUT")
            .into_iter()
            .filter(|p| !outputs.contains(p) && *p != self.path("bbl"))
            .map(|p| (p, "fls"))
            .collect();
        if engine == "lualatex" {
            deps.extend(scan::log_fonts(&log).into_iter().map(|p| (p.into(), "log")));
        }
        if engine == "xelatex" {
            deps.extend(scan::xdv_fonts(&fs::read(&xdv).unwrap_or_default()).into_iter().map(|p| (p.into(), "xdv")));
        }
        deps.extend(bib_reads.iter().cloned());
        let mut seen = std::collections::BTreeSet::new();
        deps.retain(|(p, _)| seen.insert(p.clone()));
        let inputs: Vec<Value> = deps.iter().map(|(p, via)| json!({"path": p, "via": via})).collect();
        self.emit("dep-info", json!({"inputs": inputs}));

        // An input modified after the last verification may not be what the engine read: record no
        // hash, so the next build reruns. mtimes past the build's end (clock skew, unpacked archives)
        // are not edits, or such a file would never settle. ponytail: assumes sub-second mtimes
        // (ext4, APFS, btrfs).
        let finished = SystemTime::now();
        let settled = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t < checked_at || t > finished);
        let seen: Vec<(PathBuf, Option<String>)> =
            deps.iter().map(|(p, _)| (p.clone(), settled(p).then(|| hash(p)).flatten())).collect();
        if outcome == "converged" {
            let inputs: Vec<Value> =
                deps.iter().zip(&seen).map(|((p, via), (_, h))| json!({"path": p, "via": via, "hash": h})).collect();
            for (k, v) in bib.iter_mut() {
                if bib_reads.iter().any(|(p, _)| p == Path::new(k)) && !settled(Path::new(k)) {
                    *v = None;
                }
            }
            let state = json!({"tex_root": root, "engine": engine, "pdf": hash(&self.path("pdf")), "inputs": inputs, "bib": bib});
            let _ = fs::write(&state_path, state.to_string());
        }
        let mut done = json!({"outcome": outcome, "success": outcome == "converged", "steps": self.step, "preview": preview});
        if let Some(r) = last_rerun {
            done["last_rerun"] = r;
        }
        self.emit("build-finished", done);
        self.report(outcome == "converged", Some(root), seen)
    }
}

fn lock_path(out: &Path, job: &str) -> PathBuf {
    out.join(format!(".{job}.texweft.lock"))
}

/// Removes the outputs texweft manages for `main`: the recorded outputs inside the output
/// directory, the known artifacts, and directories that held only those. The output directory and
/// the lock file stay: deleting the lock would let a waiting texweft lock an unlinked file.
fn clean(main: &Path, o: &Opts) -> bool {
    let Ok(out) = fs::canonicalize(main.parent().unwrap().join(&o.outdir)) else { return true };
    let job = main.file_stem().unwrap().to_string_lossy();
    let path = |ext: &str| out.join(format!("{job}.{ext}"));
    let waiting = |holder: Option<u32>| eprintln!("texweft: waiting for the build holding {} (pid {holder:?})", lock_path(&out, &job).display());
    let Ok(_lock) = acquire(&lock_path(&out, &job), o.lock_timeout, waiting) else {
        eprintln!("texweft: another build is still using these outputs; nothing removed");
        return false;
    };
    // Only paths that stay inside the output directory: `out/../x` starts with `out` as a path
    // prefix but names a file outside it.
    let inside = |p: &Path| p.starts_with(&out) && !p.components().any(|c| c == std::path::Component::ParentDir);
    let mut files: std::collections::BTreeSet<PathBuf> =
        scan::fls_paths(&read(&path("fls")), "OUTPUT").into_iter().filter(|p| inside(p)).collect();
    let fixed = ["fls", "log", "aux", "blg", "bcf", "run.xml", "pdf", "xdv", "synctex.gz"];
    files.extend(fixed.map(path));
    // A .bbl may be the only copy (arXiv sources ship one without the .bib). Remove it only when
    // the state shows texweft generated it with BibTeX or Biber.
    let state: Option<Value> = fs::read(path("texweft.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if state.is_some_and(|s| s["bib"].as_object().is_some_and(|b| !b.is_empty())) {
        files.insert(path("bbl"));
    } else {
        files.remove(&path("bbl"));
    }
    files.insert(path("texweft.json"));
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
