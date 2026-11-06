//! `texweft watch`: rebuild when the content of something the build read changes.
//!
//! Each build streams the same messages as `texweft build`; a `watching` message follows each one.
//! A change only counts when a content hash differs from what the build used, so `touch`,
//! rewrites with identical bytes, and the build's own outputs never trigger a rebuild.

use crate::{Build, Opts, Report, emit, find_bin, first_line, hash, shutdown};
use notify::event::{AccessKind, AccessMode};
use notify::{Config, ErrorKind, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs};

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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// `ls_r` with current hashes: tlmgr rewrites ls-R on every install, so a build that ended
    /// `missing` reruns once the caller installs the package, without restarting the watch.
    fn new(r: Report, ls_r: &BTreeMap<PathBuf, Option<String>>) -> Targets {
        let mut inputs: BTreeMap<_, _> = r.inputs.into_iter().collect();
        for (p, h) in ls_r {
            inputs.entry(p.clone()).or_insert_with(|| h.clone());
        }
        Targets { inputs, awaited: r.awaited }
    }

    fn changed(p: &Path, was: &Option<String>) -> bool {
        hash(p) != *was
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
        if p.is_file() && self.awaited.iter().any(|a| satisfies(p, a)) {
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
        self.inputs.iter().any(|(p, was)| Self::changed(p, was)) || self.awaited.iter().any(|a| present(a))
    }
}

/// Whether file `p` provides awaited path `a`. `fig` from graphicx has no extension;
/// `fig.png` appearing satisfies it.
fn satisfies(p: &Path, a: &Path) -> bool {
    p == a || (a.extension().is_none() && p.with_extension("") == a)
}

/// Whether some file now provides awaited path `a`, by the same rule as events use.
fn present(a: &Path) -> bool {
    a.is_file()
        || (a.extension().is_none()
            && a.parent().and_then(|d| fs::read_dir(d).ok()).is_some_and(|es| {
                es.flatten().any(|e| e.file_type().is_ok_and(|t| t.is_file()) && satisfies(&e.path(), a))
            }))
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
    let Some(dbs) = first_line(Command::new(bin.join("kpsewhich")).arg("-expand-path=$TEXMFDBS")) else { return Vec::new() };
    env::split_paths(&dbs)
        .map(|d| PathBuf::from(d.to_string_lossy().trim_start_matches("!!")).join("ls-R"))
        .filter(|p| p.is_file())
        .collect()
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Blocks until a rebuild is due, or until the watched set needs updating.
fn wait(rx: &Receiver<Events>, t: &Targets, watched: &mut BTreeSet<PathBuf>) -> Change {
    let (mut first, mut deadline): (Option<Instant>, Option<Instant>) = (None, None);
    // What the settle window saw: paths that looked changed, and whether the stream lost track.
    let (mut pending, mut lost) = (BTreeSet::new(), false);
    loop {
        let ev = match deadline {
            None => rx.recv_timeout(IDLE_TICK),
            Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
        };
        // `None`: the stream lost track (watcher error, queue overflow).
        let ev = ev.map(|e| e.ok().filter(|e| !e.need_rescan()));
        let change = match ev {
            // Settled: decide by content now, not by what an event saw. A save truncates then writes,
            // so the event may have seen an empty file, and A→B→A ends where it began. Only the
            // paths seen changing are compared again, unless the stream lost track.
            Err(RecvTimeoutError::Timeout) if deadline.is_some() => {
                if pending.iter().any(|p: &PathBuf| t.classify(p) == Change::Rebuild) || (lost && t.any_changed()) {
                    return Change::Rebuild;
                }
                (first, deadline, lost) = (None, None, false);
                pending.clear();
                Change::None
            }
            Err(RecvTimeoutError::Timeout) => {
                if consumer_gone() {
                    shutdown(141);
                }
                Change::None
            }
            Err(RecvTimeoutError::Disconnected) => shutdown(1),
            // The stream lost track: compare everything.
            Ok(None) => {
                lost = true;
                if t.any_changed() { Change::Rebuild } else { Change::None }
            }
            // Reads are not changes; inotify reports every open.
            Ok(Some(e)) if matches!(e.kind, EventKind::Access(a) if a != AccessKind::Close(AccessMode::Write)) => Change::None,
            Ok(Some(e)) => {
                // A watched directory that is removed loses its watch; re-add it once it is back.
                if matches!(e.kind, EventKind::Remove(_)) && e.paths.iter().any(|p| watched.remove(p)) && deadline.is_none() {
                    return Change::Resync;
                }
                let mut change = Change::None;
                for p in e.paths {
                    let c = t.classify(&p);
                    if c == Change::Rebuild {
                        pending.insert(p);
                    }
                    change = change.max(c);
                }
                change
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
    // ls-R files and their hashes, rehashed only when their mtime moves (an install rewrites them).
    let mut lsr: BTreeMap<PathBuf, (Option<SystemTime>, Option<String>)> = BTreeMap::new();
    loop {
        let report = if main.exists() {
            Build::new(main, o, force).run()
        } else {
            eprintln!("texweft: {} does not exist; waiting for it", main.display());
            Report { ok: false, inputs: Vec::new(), verified: SystemTime::now(), awaited: vec![main.to_path_buf()] }
        };
        force = false;
        if lsr.is_empty() {
            lsr = ls_r(&o.engine).into_iter().map(|p| (p, (None, None))).collect();
        }
        for (p, (m, h)) in lsr.iter_mut() {
            let now = mtime(p);
            if now.is_none() || now != *m {
                (*m, *h) = (now, hash(p));
            }
        }
        let verified = report.verified;
        let mut t = Targets::new(report, &lsr.iter().map(|(p, (_, h))| (p.clone(), h.clone())).collect());
        let added = backend.sync(&mut watched, &t);

        // Events are hints; content decides. Changes no event will report: inputs modified during
        // the build (no hash), inputs modified after the build verified them but before their
        // directory was watched, awaited files already there.
        let unseen: Vec<PathBuf> = t
            .inputs
            .iter()
            .filter(|(p, h)| {
                (h.is_none() && p.is_file())
                    || (p.parent().is_some_and(|d| added.iter().any(|a| a == d))
                        && mtime(p).is_none_or(|m| m >= verified)
                        && Targets::changed(p, h))
            })
            .map(|(p, _)| p.clone())
            .chain(t.awaited.iter().filter(|a| present(a)).cloned())
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
            if t.awaited.iter().any(|a| present(a)) {
                break;
            }
        }
    }
}
