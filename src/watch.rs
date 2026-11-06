//! `texweft watch`: rebuild when the content of something the build read changes.
//!
//! Each build streams the same messages as `texweft build`; a `watching` message follows each one.
//! A change only counts when a content hash differs from what the build used, so `touch`,
//! rewrites with identical bytes, and the build's own outputs never trigger a rebuild.

use crate::{Build, Opts, Report, emit, find_bin, hash, shutdown};
use notify::event::{AccessKind, AccessMode};
use notify::{Config, ErrorKind, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use std::env;

/// Quiet time after the last relevant event before building: editors save in several writes.
const SETTLE: Duration = Duration::from_millis(100);
/// Upper bound on that wait when relevant events never stop.
const SETTLE_MAX: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How often an idle watch checks that its consumer is still reading stdout.
const IDLE_TICK: Duration = Duration::from_millis(500);
/// Rebuilds in a row a path may cause without an event: modified during the build, changed before
/// its directory was watched, or an awaited file already present. Past this, the build itself
/// keeps causing it (or the file is unusable), and only real events count.
const UNSEEN_RETRIES: u32 = 2;

type Events = notify::Result<Event>;

#[derive(PartialEq, PartialOrd)]
enum Change {
    None,
    Resync,
    Rebuild,
}

struct Targets {
    /// Inputs and the hash the last build used; `None` when it could not tell.
    inputs: BTreeMap<PathBuf, Option<String>>,
    /// Paths that do not exist yet but may fix the build.
    awaited: Vec<PathBuf>,
}

impl Targets {
    fn new(r: Report, ls_r: Vec<PathBuf>) -> Targets {
        let mut inputs: BTreeMap<_, _> = r.inputs.into_iter().collect();
        // tlmgr rewrites ls-R on every install, so a build that ended `missing` reruns once the
        // caller installs the package, without the caller having to restart the watch.
        for p in ls_r {
            let h = hash(&p);
            inputs.entry(p).or_insert(h);
        }
        Targets { inputs, awaited: r.awaited }
    }

    fn changed(p: &Path, was: &Option<String>) -> bool {
        let now = hash(p);
        match was {
            Some(_) => now != *was,
            None => now.is_some(),
        }
    }

    /// Where each input lives, and the nearest existing ancestor of each awaited path.
    fn dirs(&self) -> BTreeSet<PathBuf> {
        let mut d: BTreeSet<_> = self.inputs.keys().filter_map(|p| p.parent()).map(Path::to_path_buf).collect();
        d.extend(self.awaited.iter().filter_map(|a| a.ancestors().skip(1).find(|p| p.is_dir())).map(Path::to_path_buf));
        d
    }

    fn classify(&self, p: &Path) -> Change {
        if let Some(was) = self.inputs.get(p) {
            return if Self::changed(p, was) { Change::Rebuild } else { Change::None };
        }
        // `fig` from graphicx has no extension; `fig.png` appearing satisfies it.
        if p.is_file() && self.awaited.iter().any(|a| a == p || (a.extension().is_none() && p.with_extension("") == *a)) {
            return Change::Rebuild;
        }
        // A directory on the way to an awaited path appeared: watch it, nothing to build yet.
        if p.is_dir() && self.awaited.iter().any(|a| a.starts_with(p) && a != p) {
            return Change::Resync;
        }
        Change::None
    }

    /// Full comparison, for when the event stream lost track (queue overflow, watcher error).
    fn any_changed(&self) -> bool {
        self.inputs.iter().any(|(p, was)| Self::changed(p, was)) || self.awaited.iter().any(|a| a.exists())
    }
}

struct Backend {
    watcher: Box<dyn Watcher>,
    poll: bool,
    tx: Sender<Events>,
}

impl Backend {
    fn new(tx: Sender<Events>, poll: bool) -> Backend {
        if !poll {
            match RecommendedWatcher::new(tx.clone(), Config::default()) {
                Ok(w) => return Backend { watcher: Box::new(w), poll: false, tx },
                Err(e) => eprintln!("texweft: native file events unavailable ({e}); polling instead"),
            }
        }
        Backend::polling(tx)
    }

    fn polling(tx: Sender<Events>) -> Backend {
        let w = PollWatcher::new(tx.clone(), Config::default().with_poll_interval(POLL_INTERVAL));
        let w = w.unwrap_or_else(|e| {
            eprintln!("texweft: cannot watch files: {e}");
            std::process::exit(1)
        });
        Backend { watcher: Box::new(w), poll: true, tx }
    }

    /// Makes the watched directory set equal to what `t` needs; returns the directories added.
    fn sync(&mut self, watched: &mut BTreeSet<PathBuf>, t: &Targets) -> Vec<PathBuf> {
        let mut added = Vec::new();
        let want = t.dirs();
        for d in watched.difference(&want).cloned().collect::<Vec<_>>() {
            let _ = self.watcher.unwatch(&d);
            watched.remove(&d);
        }
        for d in want.difference(&watched.clone()) {
            match self.watcher.watch(d, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched.insert(d.clone());
                    added.push(d.clone());
                }
                Err(e) if !self.poll && matches!(e.kind, ErrorKind::MaxFilesWatch) => {
                    eprintln!("texweft: OS file watch limit reached; polling instead");
                    *self = Backend::polling(self.tx.clone());
                    watched.clear();
                    return self.sync(watched, t);
                }
                Err(_) => {} // gone since the build; retried on the next sync
            }
        }
        added
    }
}

/// Every ls-R kpathsea consults, from `$TEXMFDBS`.
fn ls_r(engine: &str) -> Vec<PathBuf> {
    let Some(bin) = find_bin(engine) else { return Vec::new() };
    let Ok(out) = Command::new(bin.join("kpsewhich")).arg("-expand-path=$TEXMFDBS").output() else { return Vec::new() };
    let dbs = String::from_utf8_lossy(&out.stdout).trim().to_string();
    env::split_paths(&dbs)
        .map(|d| PathBuf::from(d.to_string_lossy().trim_start_matches("!!")).join("ls-R"))
        .filter(|p| p.is_file())
        .collect()
}

/// Blocks until a rebuild is due, or until the watched set needs updating.
fn wait(rx: &Receiver<Events>, t: &Targets, watched: &mut BTreeSet<PathBuf>) -> Change {
    let (mut first, mut deadline): (Option<Instant>, Option<Instant>) = (None, None);
    loop {
        let ev = match deadline {
            None => rx.recv_timeout(IDLE_TICK),
            Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
        };
        let change = match ev {
            Err(RecvTimeoutError::Timeout) if deadline.is_some() => return Change::Rebuild,
            Err(RecvTimeoutError::Timeout) => {
                if consumer_gone() {
                    shutdown(141);
                }
                Change::None
            }
            Err(RecvTimeoutError::Disconnected) => shutdown(1),
            Ok(Err(_)) => {
                if t.any_changed() { Change::Rebuild } else { Change::None }
            }
            // Reads are not changes; inotify reports every open.
            Ok(Ok(e)) if matches!(e.kind, EventKind::Access(a) if a != AccessKind::Close(AccessMode::Write)) => Change::None,
            Ok(Ok(e)) if e.need_rescan() => {
                if t.any_changed() { Change::Rebuild } else { Change::None }
            }
            Ok(Ok(e)) => {
                // A watched directory that is removed loses its watch; re-add it once it is back.
                if matches!(e.kind, EventKind::Remove(_)) && e.paths.iter().any(|p| watched.remove(p)) && deadline.is_none() {
                    return Change::Resync;
                }
                e.paths.iter().map(|p| t.classify(p)).fold(Change::None, |a, b| if b > a { b } else { a })
            }
        };
        match change {
            Change::Rebuild => {
                let now = Instant::now();
                let first = *first.get_or_insert(now);
                deadline = Some((now + SETTLE).min(first + SETTLE_MAX));
            }
            Change::Resync if deadline.is_none() => return Change::Resync,
            _ => {}
        }
    }
}

/// Whether the reader of stdout has gone away (pipe closed), so an idle watch serves nobody.
/// A terminal, a file, or /dev/null never reports this.
fn consumer_gone() -> bool {
    #[cfg(unix)]
    {
        let mut p = libc::pollfd { fd: 1, events: 0, revents: 0 };
        unsafe { libc::poll(&mut p, 1, 0) > 0 && p.revents & (libc::POLLERR | libc::POLLHUP) != 0 }
    }
    #[cfg(not(unix))]
    false
}

pub fn run(main: &Path, o: &Opts, force: bool) -> ! {
    let (tx, rx) = mpsc::channel();
    let mut backend = Backend::new(tx, o.poll);
    let mut watched = BTreeSet::new();
    let mut force = force;
    let mut unseen_runs: BTreeMap<PathBuf, u32> = BTreeMap::new();
    loop {
        let report = if main.exists() {
            Build::new(main, o, force).run()
        } else {
            eprintln!("texweft: {} does not exist; waiting for it", main.display());
            Report { ok: false, tex_root: None, inputs: Vec::new(), awaited: vec![main.to_path_buf()] }
        };
        force = false;
        let mut t = Targets::new(report, ls_r(&o.engine));
        let added = backend.sync(&mut watched, &t);

        // Events are hints; content decides. Changes no event will report: inputs modified during
        // the build (no hash), inputs in directories watched only now, awaited files already there.
        let unseen: Vec<PathBuf> = t
            .inputs
            .iter()
            .filter(|(p, h)| {
                (h.is_none() && p.is_file()) || (p.parent().is_some_and(|d| added.iter().any(|a| a == d)) && Targets::changed(p, h))
            })
            .map(|(p, _)| p.clone())
            .chain(t.awaited.iter().filter(|a| a.is_file()).cloned())
            .collect();
        unseen_runs.retain(|p, _| unseen.contains(p));
        let mut again = false;
        for p in unseen {
            let n = unseen_runs.entry(p.clone()).or_insert(0);
            *n += 1;
            if *n <= UNSEEN_RETRIES {
                again = true;
            } else if let Some(h) = t.inputs.get_mut(&p) {
                *h = hash(&p);
            } else {
                t.awaited.retain(|a| *a != p);
            }
        }

        let backend_name = if backend.poll { "poll" } else { "native" };
        emit(o.json, "watching", json!({"inputs": t.inputs.len(), "dirs": watched.len(), "backend": backend_name}));
        if again {
            continue;
        }
        // A directory on the way to an awaited file appeared; the file may already be inside it.
        while wait(&rx, &t, &mut watched) == Change::Resync {
            backend.sync(&mut watched, &t);
            if t.awaited.iter().any(|a| a.is_file()) {
                break;
            }
        }
    }
}
