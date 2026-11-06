//! Behavioural contract of `texweft build/watch/clean` against a real TeX installation.
//!
//! These check what texweft promises (outcomes, what the final run read, what clean keeps), not
//! timings or byte-identical PDFs, and allow texweft to reach a correct result in fewer steps.
//! They need pdfLaTeX and BibTeX: on PATH, or in the directory named by TEXWEFT_TEST_TEXBIN.
//! Without them every test is skipped with a note.

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs};

const TEXWEFT: &str = env!("CARGO_BIN_EXE_texweft");

fn tex_bin() -> Option<PathBuf> {
    if let Some(d) = env::var_os("TEXWEFT_TEST_TEXBIN") {
        return Some(d.into());
    }
    env::split_paths(&env::var_os("PATH")?).find(|d| d.join("pdflatex").is_file() && d.join("bibtex").is_file())
}

macro_rules! need_tex {
    () => {
        match tex_bin() {
            Some(b) => b,
            None => {
                eprintln!("skipped: no pdflatex/bibtex on PATH (set TEXWEFT_TEST_TEXBIN)");
                return;
            }
        }
    };
}

struct Doc {
    dir: PathBuf,
    path: String,
}

impl Doc {
    fn new(name: &str, bin: &Path) -> Doc {
        let dir = env::temp_dir().join(format!("texweft-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())))
            .unwrap()
            .into_string()
            .unwrap();
        Doc { dir, path }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.join(rel)).unwrap_or_default()
    }

    fn cmd(&self, sub: &str, args: &[&str]) -> Command {
        let mut c = Command::new(TEXWEFT);
        c.arg(sub).arg("main.tex").arg("--message-format=json").args(args).current_dir(&self.dir).env("PATH", &self.path);
        c
    }

    fn build(&self, args: &[&str]) -> Vec<Value> {
        self.build_with(self.cmd("build", args))
    }

    fn build_with(&self, mut c: Command) -> Vec<Value> {
        let out = c.output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn clean(&self, args: &[&str]) {
        let mut c = Command::new(TEXWEFT);
        c.arg("clean").arg("main.tex").args(args).current_dir(&self.dir).env("PATH", &self.path);
        assert!(c.status().unwrap().success());
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The JSON messages of a running `watch`, read on a thread so tests can time out.
fn messages(out: std::process::ChildStdout) -> std::sync::mpsc::Receiver<Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str(&l).unwrap()).is_err() {
                break;
            }
        }
    });
    rx
}

/// Messages up to and including the next `watching`, or whatever arrived before `wait` ran out.
fn until_watching(rx: &std::sync::mpsc::Receiver<Value>, wait: Duration) -> Vec<Value> {
    let end = Instant::now() + wait;
    let mut got = Vec::new();
    while let Ok(m) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        let done = m["type"] == "watching";
        got.push(m);
        if done {
            break;
        }
    }
    got
}

fn finished(msgs: &[Value]) -> &Value {
    msgs.iter().rfind(|m| m["type"] == "build-finished").expect("build-finished")
}

fn steps<'a>(msgs: &'a [Value], tool: &str) -> Vec<&'a Value> {
    msgs.iter().filter(|m| m["type"] == "step-started" && m["tool"] == tool).collect()
}

/// A directory shadowing `bin`: every program links to it except `pdflatex`, which runs `script`.
fn shadow_bin(name: &str, bin: &Path, script: &str) -> PathBuf {
    let d = env::temp_dir().join(format!("texweft-test-bin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    for e in fs::read_dir(bin).unwrap().flatten() {
        if e.file_name() != "pdflatex" {
            std::os::unix::fs::symlink(e.path(), d.join(e.file_name())).unwrap();
        }
    }
    let p = d.join("pdflatex");
    fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(&mut fs::metadata(&p).unwrap().permissions(), 0o755);
    fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    d
}

const PAPER: &str = "\\documentclass{article}
\\begin{document}
\\section{A}\\label{a} See \\ref{a} and \\cite{knuth}.
\\include{ch/one}
\\bibliographystyle{plain}\\bibliography{refs}
\\end{document}
";
const BIB: &str = "@book{knuth, author={D. Knuth}, title={T}, year={1984}, publisher={AW}}\n";

fn paper(name: &str, bin: &Path) -> Doc {
    let d = Doc::new(name, bin);
    d.write("main.tex", PAPER);
    d.write("ch/one.tex", "Chapter.\n");
    d.write("refs.bib", BIB);
    d
}

#[test]
fn converges_into_build_dir_then_is_up_to_date() {
    let bin = need_tex!();
    let d = paper("converge", &bin);
    let m = d.build(&[]);
    assert_eq!(finished(&m)["outcome"], "converged");
    assert!(d.dir.join("build/ch/one.aux").is_file(), "\\include subdirectory created in build/");
    assert!(d.read("build/main.aux").contains("\\bibcite{knuth}"));
    assert!(!d.dir.join("main.aux").exists(), "sources stay clean");
    let m = d.build(&[]);
    assert_eq!((finished(&m)["outcome"].clone(), finished(&m)["steps"].clone()), ("converged".into(), 0.into()));
}

#[test]
fn an_error_still_completes_citations_but_fails() {
    let bin = need_tex!();
    let d = paper("error", &bin);
    d.write("ch/one.tex", "Chapter \\undefinedmacro.\n");
    let m = d.build(&[]);
    let f = finished(&m);
    assert_eq!(f["outcome"], "failed");
    assert!(!f["preview"].is_null(), "a fresh PDF is still offered");
    assert_eq!(steps(&m, "bibtex").len(), 1, "BibTeX ran despite the error");
    assert!(d.read("build/main.aux").contains("\\bibcite{knuth}"));
    assert!(m.iter().any(|x| x["type"] == "diagnostic" && x["code"] == "undefined-control-sequence"));
}

#[test]
fn an_input_saved_during_the_build_is_read_before_it_converges() {
    let bin = need_tex!();
    let d = Doc::new("race", &bin);
    d.write("main.tex", "\\documentclass{article}\\begin{document}\\input{body}\\end{document}\n");
    d.write("body.tex", "\\typeout{BODY:before}Body.\n");
    let flag = d.dir.join("save-once");
    fs::write(&flag, "").unwrap();
    // After the engine has read body.tex, an editor saves a new version.
    let script = format!(
        "{}/pdflatex \"$@\"; rc=$?\n[ -f {f} ] && {{ rm {f}; printf '\\\\typeout{{BODY:after}}Body.\\n' > body.tex; }}\nexit $rc",
        bin.display(),
        f = flag.display()
    );
    let shadow = shadow_bin("race", &bin, &script);
    let mut c = d.cmd("build", &[]);
    c.env("PATH", format!("{}:{}", shadow.display(), d.path));
    let m = d.build_with(c);
    assert_eq!(finished(&m)["outcome"], "converged");
    assert!(d.read("build/main.log").contains("BODY:after"), "the converged run read the saved version");
    assert_eq!(finished(&d.build(&[]))["steps"], 0, "and the next build trusts it");
    let _ = fs::remove_dir_all(shadow);
}

#[test]
fn an_engine_that_dies_before_its_log_is_not_judged_by_the_old_log() {
    let bin = need_tex!();
    let d = Doc::new("stale", &bin);
    d.write("main.tex", "\\documentclass{article}\\usepackage{zzmissing}\\begin{document}x\\end{document}\n");
    assert_eq!(finished(&d.build(&[]))["outcome"], "missing");
    let script = "echo 'pdflatex: error while loading shared libraries: libfoo.so.1: cannot open shared object file' >&2\nexit 127";
    let shadow = shadow_bin("stale", &bin, script);
    let mut c = d.cmd("build", &[]);
    c.env("PATH", format!("{}:{}", shadow.display(), d.path));
    let m = d.build_with(c);
    let missing: Vec<_> = m.iter().filter(|x| x["type"] == "missing").map(|x| (x["kind"].clone(), x["name"].clone())).collect();
    assert_eq!(missing, [("library".into(), "libfoo.so.1".into())]);
    let _ = fs::remove_dir_all(shadow);
}

#[test]
fn an_installed_package_is_picked_up_without_force() {
    let bin = need_tex!();
    let d = Doc::new("install", &bin);
    let home = d.dir.join("texmf");
    d.write("main.tex", "\\documentclass{article}\\usepackage{zzpkg}\\begin{document}\\zz\\end{document}\n");
    let build = || {
        let mut c = d.cmd("build", &[]);
        c.env("TEXMFHOME", &home);
        d.build_with(c)
    };
    assert_eq!(finished(&build())["outcome"], "missing");
    // tlmgr unpacks files with their original, old mtimes.
    d.write("texmf/tex/latex/zzpkg/zzpkg.sty", "\\ProvidesPackage{zzpkg}\\newcommand\\zz{ok}\n");
    let old = fs::File::options().write(true).open(home.join("tex/latex/zzpkg/zzpkg.sty")).unwrap();
    old.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_577_836_800)).unwrap();
    assert_eq!(finished(&build())["outcome"], "converged");
}

#[test]
fn concurrent_builds_of_one_output_dir_are_serialized() {
    let bin = need_tex!();
    let d = paper("concurrent", &bin);
    // Slow enough that the second build starts while the first holds the lock.
    d.write("ch/one.tex", "\\count255=0 \\loop\\advance\\count255 by1 \\ifnum\\count255<3000000 \\repeat Chapter.\n");
    d.build(&[]);
    let spawn = || d.cmd("build", &["--force"]).stdout(Stdio::piped()).spawn().unwrap();
    let (a, b) = (spawn(), spawn());
    let outs: Vec<String> = [a, b].into_iter().map(|c| String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout).into_owned()).collect();
    for o in &outs {
        let m: Vec<Value> = o.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(finished(&m)["outcome"], "converged");
    }
    assert!(outs.iter().any(|o| o.contains("\"lock-wait\"")), "one build waited for the other");
    assert!(d.read("build/main.aux").contains("\\bibcite{knuth}"));
}

#[test]
fn a_document_that_never_settles_stops_at_the_limit() {
    let bin = need_tex!();
    let d = Doc::new("limit", &bin);
    d.write(
        "main.tex",
        "\\documentclass{article}\\makeatletter\\providecommand\\runcount{0}
\\AtEndDocument{\\immediate\\write\\@auxout{\\gdef\\string\\runcount{\\the\\numexpr\\runcount+1\\relax}}}
\\makeatother\\begin{document}\\runcount\\end{document}\n",
    );
    let m = d.build(&[]);
    assert_eq!(finished(&m)["outcome"], "not-converged");
    assert_eq!(finished(&m)["last_rerun"]["kind"], "inputs-changed");
    assert_eq!(steps(&m, "pdflatex").len(), 5);
}

#[test]
fn a_shipped_bbl_is_used_when_the_bib_is_absent() {
    let bin = need_tex!();
    let d = Doc::new("arxiv", &bin);
    d.write("main.tex", "\\documentclass{article}\\begin{document}See \\cite{knuth}.\\bibliographystyle{plain}\\bibliography{refs}\\end{document}\n");
    d.write("main.bbl", "\\begin{thebibliography}{1}\\bibitem{knuth} Knuth.\\end{thebibliography}\n");
    let m = d.build(&[]);
    assert_eq!(finished(&m)["outcome"], "converged");
    assert!(steps(&m, "bibtex").is_empty());
    let sub: Vec<_> = m.iter().filter(|x| x["type"] == "missing").map(|x| (x["name"].clone(), x["fatal"].clone(), x["substitute"].clone())).collect();
    assert_eq!(sub, [("refs.bib".into(), false.into(), "bbl".into())]);
}

#[test]
fn clean_removes_only_what_texweft_made() {
    let bin = need_tex!();
    // Generated .bbl, user files in build/, and a forged .fls entry pointing outside build/.
    let d = paper("clean", &bin);
    assert_eq!(finished(&d.build(&[]))["outcome"], "converged");
    d.write("build/notes.txt", "mine\n");
    d.write("build/keep/x.txt", "mine\n");
    d.write("victim.txt", "mine\n");
    let fls = d.read("build/main.fls") + "OUTPUT build/../victim.txt\n";
    fs::write(d.dir.join("build/main.fls"), fls).unwrap();
    d.clean(&[]);
    for kept in ["build/notes.txt", "build/keep/x.txt", "victim.txt", "main.tex", "refs.bib", "ch/one.tex"] {
        assert!(d.dir.join(kept).exists(), "{kept} kept");
    }
    for gone in ["build/main.aux", "build/main.bbl", "build/main.pdf", "build/ch"] {
        assert!(!d.dir.join(gone).exists(), "{gone} removed");
    }

    // In-source outputs with a shipped .bbl that texweft did not generate.
    let a = Doc::new("clean-arxiv", &bin);
    a.write("main.tex", "\\documentclass{article}\\begin{document}See \\cite{knuth}.\\bibliographystyle{plain}\\bibliography{refs}\\end{document}\n");
    a.write("main.bbl", "\\begin{thebibliography}{1}\\bibitem{knuth} Knuth.\\end{thebibliography}\n");
    assert_eq!(finished(&a.build(&["--outdir", "."]))["outcome"], "converged");
    a.clean(&["--outdir", "."]);
    assert!(a.dir.join("main.bbl").exists(), "the only copy of the bibliography is kept");
    assert!(!a.dir.join("main.aux").exists());
}

#[test]
fn watch_rebuilds_on_content_changes_only() {
    let bin = need_tex!();
    let d = Doc::new("watch", &bin);
    d.write("main.tex", "\\documentclass{article}\\begin{document}\\input{body}\\end{document}\n");
    d.write("body.tex", "One.\n");
    let mut child = d.cmd("watch", &[]).stdout(Stdio::piped()).spawn().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str(&l).unwrap()).is_err() {
                break;
            }
        }
    });
    // Messages until the next `watching`, or None if nothing arrives within `wait`.
    let next = |wait: Duration| -> Option<Vec<Value>> {
        let end = Instant::now() + wait;
        let mut got = Vec::new();
        while let Ok(m) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            let done = m["type"] == "watching";
            got.push(m);
            if done {
                return Some(got);
            }
        }
        (!got.is_empty()).then_some(got)
    };
    assert_eq!(finished(&next(Duration::from_secs(60)).unwrap())["outcome"], "converged");
    // touch and an identical rewrite are not changes.
    fs::File::options().write(true).open(d.dir.join("body.tex")).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
    d.write("body.tex", "One.\n");
    let unexpected = next(Duration::from_secs(2));
    assert!(unexpected.is_none(), "no rebuild without a content change: {unexpected:?}");
    d.write("body.tex", "Two.\n");
    let m = next(Duration::from_secs(60)).unwrap();
    let why = &steps(&m, "pdflatex")[0]["rerun"];
    assert_eq!(why["kind"], "inputs-changed");
    assert!(why["files"][0]["path"].as_str().unwrap().ends_with("body.tex"));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143), "{status:?}");
}

#[test]
fn an_input_deleted_during_the_build_is_not_converged() {
    let bin = need_tex!();
    let d = Doc::new("vanish", &bin);
    d.write("main.tex", "\\documentclass{article}\\begin{document}Main.\\end{document}\n");
    assert_eq!(finished(&d.build(&[]))["outcome"], "converged");
    // A new input appears; the engine reads it, then it is deleted before the build ends. Nothing
    // else asks for another run (.aux unchanged), so only verification can notice.
    d.write("main.tex", "\\documentclass{article}\\begin{document}Main.\\input{extra}\\end{document}\n");
    d.write("extra.tex", "Extra.\n");
    let flag = d.dir.join("delete-once");
    fs::write(&flag, "").unwrap();
    let script = format!("{}/pdflatex \"$@\"; rc=$?\n[ -f {f} ] && {{ rm {f} extra.tex; }}\nexit $rc", bin.display(), f = flag.display());
    let shadow = shadow_bin("vanish", &bin, &script);
    let mut c = d.cmd("build", &[]);
    c.env("PATH", format!("{}:{}", shadow.display(), d.path));
    let m = d.build_with(c);
    assert_ne!(finished(&m)["outcome"], "converged", "the PDF was built from a file that is gone");
    assert_ne!(finished(&d.build(&[]))["steps"], 0);
    let _ = fs::remove_dir_all(shadow);
}

#[test]
fn watch_rebuilds_when_a_missing_image_appears_with_an_extension() {
    let bin = need_tex!();
    let img = Command::new(bin.join("kpsewhich")).arg("example-image-a.pdf").output().unwrap();
    let img = String::from_utf8_lossy(&img.stdout).trim().to_string();
    if img.is_empty() {
        eprintln!("skipped: example-image-a.pdf (mwe) not installed");
        return;
    }
    let d = Doc::new("watch-img", &bin);
    d.write("main.tex", "\\documentclass{article}\\usepackage{graphicx}\\begin{document}\\includegraphics{fig}\\end{document}\n");
    let mut child = d.cmd("watch", &[]).stdout(Stdio::piped()).spawn().unwrap();
    let rx = messages(child.stdout.take().unwrap());
    let first = until_watching(&rx, Duration::from_secs(60));
    fs::copy(&img, d.dir.join("fig.pdf")).unwrap();
    let second = until_watching(&rx, Duration::from_secs(10));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap();
    assert_eq!(finished(&first)["outcome"], "missing");
    assert!(second.iter().any(|m| m["type"] == "build-finished"), "no rebuild after fig.pdf appeared");
    assert_eq!(finished(&second)["outcome"], "converged");
}

const CITE: &str = "\\documentclass{article}\\begin{document}See \\cite{knuth}.\\bibliographystyle{plain}\\bibliography{refs}\\end{document}\n";

#[test]
fn a_shipped_bbl_at_the_output_path_is_still_an_input() {
    let bin = need_tex!();
    let d = Doc::new("bbl-inplace", &bin);
    d.write("main.tex", CITE);
    d.write("main.bbl", "\\begin{thebibliography}{1}\\bibitem{knuth} First.\\end{thebibliography}\n");
    assert_eq!(finished(&d.build(&["--outdir", "."]))["outcome"], "converged");
    d.write("main.bbl", "\\begin{thebibliography}{1}\\bibitem{knuth} Second.\\end{thebibliography}\n");
    let m = d.build(&["--outdir", "."]);
    assert_eq!(finished(&m)["outcome"], "converged");
    assert_ne!(finished(&m)["steps"], 0, "editing the shipped .bbl rebuilds");
}

#[test]
fn a_shipped_bbl_is_not_shadowed_by_an_old_generated_one() {
    let bin = need_tex!();
    let d = Doc::new("bbl-shadow", &bin);
    d.write("main.tex", CITE);
    d.write("refs.bib", BIB);
    assert_eq!(finished(&d.build(&[]))["outcome"], "converged");
    assert!(d.dir.join("build/main.bbl").exists());
    fs::remove_file(d.dir.join("refs.bib")).unwrap();
    d.write("main.bbl", "\\begin{thebibliography}{1}\\bibitem{knuth} \\typeout{SHIPPED-BBL}Shipped.\\end{thebibliography}\n");
    let m = d.build(&[]);
    assert_eq!(finished(&m)["outcome"], "converged");
    assert!(!d.dir.join("build/main.bbl").exists(), "the generated .bbl no longer hides the shipped one");
    assert!(d.read("build/main.log").contains("SHIPPED-BBL"), "LaTeX read the shipped .bbl");
}

#[test]
fn clean_does_not_follow_directory_symlinks_out_of_the_output_dir() {
    let bin = need_tex!();
    let d = paper("clean-link", &bin);
    assert_eq!(finished(&d.build(&[]))["outcome"], "converged");
    let outside = d.dir.join("outside");
    d.write("outside/victim.txt", "mine\n");
    std::os::unix::fs::symlink(&outside, d.dir.join("build/link")).unwrap();
    let fls = d.read("build/main.fls") + &format!("OUTPUT {}/build/link/victim.txt\n", d.dir.display());
    fs::write(d.dir.join("build/main.fls"), fls).unwrap();
    d.clean(&[]);
    assert!(outside.join("victim.txt").exists(), "a file reached through a symlinked directory is kept");
}
