use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    FontFile,
    FontName,
    FontOutline,
    /// A tool that is not installed (`biber`, `xdvipdfmx`, the engine itself).
    Program,
    /// A shared library the dynamic loader could not find; a system package, not TeX Live.
    Library,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Missing {
    pub kind: Kind,
    pub name: String,
    pub fatal: bool,
    pub substitute: Option<&'static str>,
    pub raw: Vec<String>,
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Text between `pre` and the next `'`, if `post` follows it.
pub fn quoted<'a>(line: &'a str, pre: &str, post: &str) -> Option<&'a str> {
    let rest = &line[line.find(pre)? + pre.len()..];
    let j = rest.find('\'')?;
    rest[j..].starts_with(post).then(|| &rest[..j])
}

fn unloadable(line: &str) -> Option<&str> {
    let head = &line[..line.find(" not loadable: ")?];
    Some(head.split(" at ").next().unwrap_or(head).split_once('=')?.1)
}

pub fn missing(log: &str) -> Vec<Missing> {
    let mut out: Vec<Missing> = Vec::new();
    let mut push = |kind, name: String, fatal, substitute, line: &str| {
        if !out.iter().any(|m| m.kind == kind && m.name == name) {
            out.push(Missing {
                kind,
                name,
                fatal,
                substitute,
                raw: vec![line.to_string()],
            });
        }
    };
    for line in log.lines() {
        // LaTeX's \@missingfileerror always prints `base.ext`; graphicx prints the
        // name as written. Keep both verbatim. The `LaTeX Warning:` variant is skipped.
        if let Some(f) = quoted(line, "Error: File `", "' not found") {
            push(Kind::File, f.to_string(), true, None, line);
        }
        // TeX's \input primitive: kpathsea tries `.tex` when the base name has none.
        if let Some(f) = quoted(line, "I can't find file `", "'") {
            let f = if Path::new(f).extension().is_some() {
                f.to_string()
            } else {
                format!("{f}.tex")
            };
            push(Kind::File, f, true, None, line);
        }
        if let Some(name) = unloadable(line) {
            if let Some(f) = name
                .strip_prefix('[')
                .and_then(|n| n.split_once(']'))
                .map(|(f, _)| f)
            {
                push(Kind::FontFile, f.to_string(), true, None, line);
            } else if let Some(n) = name
                .strip_prefix('"')
                .and_then(|n| n.strip_suffix('"'))
                .or_else(|| name.strip_prefix("name:"))
                .and_then(|n| n.split([':', '/']).next())
            // drop `/OT` or `:features`
            {
                push(Kind::FontName, n.to_string(), true, None, line);
            } else if !name.is_empty() && name.chars().all(is_name) {
                push(Kind::FontFile, format!("{name}.tfm"), true, None, line);
            }
        }
        // pdfTeX without METAFONT: `!pdfTeX error: pdflatex (file tcrm1200): Font tcrm1200 at 600 not found`
        if let Some(rest) = line.split_once("): Font ").map(|(_, r)| r)
            && let Some((name, _)) = rest.split_once(" at ")
            && line.ends_with(" not found")
        {
            push(Kind::FontOutline, name.to_string(), true, None, line);
        }
        // pdfTeX with METAFONT: a bitmap `/<name>.<dpi>pk>` silently replaced the outline font.
        for (i, _) in line.match_indices("pk>") {
            let s = line[..i].trim_end_matches(|c: char| c.is_ascii_digit());
            if let Some(s) = s.strip_suffix('.') {
                let name = &s[s.trim_end_matches(is_name).len()..];
                if !name.is_empty() && s[..s.len() - name.len()].ends_with('/') {
                    push(Kind::FontOutline, name.to_string(), false, Some("pk"), line);
                }
            }
        }
    }
    out
}

/// The loader error a program prints when it cannot start, before any TeX log exists.
pub fn missing_library(output: &str) -> Option<Missing> {
    let line = output.lines().find(|l| l.contains("error while loading shared libraries: "))?;
    let name = line.split("error while loading shared libraries: ").nth(1)?.split(':').next()?;
    Some(Missing { kind: Kind::Library, name: name.to_string(), fatal: true, substitute: None, raw: vec![line.to_string()] })
}

/// LuaTeX reports font files only as `</path/x.otf>` in the log.
pub fn log_fonts(log: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for part in log
        .lines()
        .filter(|l| l.starts_with("</"))
        .flat_map(|l| l.split('<'))
    {
        if let Some(p) = part
            .split('>')
            .next()
            .filter(|p| p.starts_with('/') && p.contains("/fonts/"))
        {
            out.insert(p.to_string());
        }
    }
    out
}

// XDV opcode 252: k[4] size[4] flags[2] len[1] path[len].
pub fn xdv_fonts(xdv: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in xdv.iter().enumerate().filter(|(_, b)| **b == 252) {
        let Some(&len) = xdv.get(i + 11) else {
            continue;
        };
        if let Some(Ok(p)) = xdv
            .get(i + 12..i + 12 + len as usize)
            .map(std::str::from_utf8)
            && p.starts_with('/')
            && p.contains("/fonts/")
        {
            out.insert(p.to_string());
        }
    }
    out
}

/// `INPUT` or `OUTPUT` paths from a `-recorder` `.fls`, resolved against its `PWD`.
pub fn fls_paths(fls: &str, kind: &str) -> BTreeSet<PathBuf> {
    let pwd = fls
        .lines()
        .find_map(|l| l.strip_prefix("PWD "))
        .unwrap_or("");
    fls.lines()
        .filter_map(|l| l.strip_prefix(kind)?.strip_prefix(' '))
        .map(|p| Path::new(pwd).join(p).components().collect())
        .collect()
}

#[derive(Debug)]
pub struct Diag {
    pub level: &'static str,
    pub code: Option<&'static str>,
    pub message: String,
    /// File as the log names it (relative to the engine's cwd), and line.
    pub span: Option<(Option<String>, u32)>,
    pub raw: Vec<String>,
}

/// `./sec/a.tex:12: message`, as printed under `-file-line-error`.
fn file_line(l: &str) -> Option<(&str, u32, &str)> {
    if !(l.starts_with("./") || l.starts_with("../") || l.starts_with('/')) {
        return None;
    }
    l.match_indices(':').find_map(|(i, _)| {
        let rest = &l[i + 1..];
        let n = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        (n > 0 && rest[n..].starts_with(": "))
            .then(|| Some((&l[..i], rest[..n].parse().ok()?, &rest[n + 2..])))?
    })
}

/// The number after the first `pat` in `s`.
fn num_after(s: &str, pat: &str) -> Option<u32> {
    let rest = &s[s.find(pat)? + pat.len()..];
    rest[..rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len()]
        .parse()
        .ok()
}

fn code(msg: &str) -> Option<&'static str> {
    let undefined = msg.contains("undefined");
    if msg.starts_with("Undefined control sequence") {
        Some("undefined-control-sequence")
    } else if undefined && msg.contains("Reference `") {
        Some("undefined-reference")
    } else if undefined && (msg.contains("Citation `") || msg.contains("Citation '")) {
        Some("undefined-citation")
    } else if msg.starts_with("Overfull") {
        Some("overfull-box")
    } else if msg.starts_with("Underfull") {
        Some("underfull-box")
    } else {
        None
    }
}

pub fn diagnostics(log: &str) -> Vec<Diag> {
    let lines: Vec<&str> = log.lines().collect();
    let mut out = Vec::new();
    for (i, &l) in lines.iter().enumerate() {
        let error = file_line(l)
            .map(|(f, n, m)| (Some((Some(f.to_string()), n)), m.trim_start()))
            .or_else(|| l.strip_prefix('!').map(|m| (None, m.trim_start())));
        if let Some((span, msg)) = error {
            // Context runs to the `l.<n>` line and the continuation after it.
            let end = lines[i + 1..]
                .iter()
                .take(10)
                .position(|x| x.starts_with("l."))
                .map_or(i + 1, |k| (i + k + 3).min(lines.len()));
            let raw = lines[i..end].iter().map(|s| s.to_string()).collect();
            out.push(Diag {
                level: "error",
                code: code(msg),
                message: msg.to_string(),
                span,
                raw,
            });
        } else if let Some(k) = l.find(" Warning: ")
            && let head = &l[..k]
            && (head.starts_with("LaTeX")
                || head.starts_with("Package ")
                || head.starts_with("Class "))
        {
            // \MessageBreak continuations are prefixed `(pkg)`.
            let tag = format!("({})", head.rsplit(' ').next().unwrap_or(head));
            let more: Vec<&str> = lines[i + 1..]
                .iter()
                .copied()
                .take_while(|x| x.starts_with(&tag))
                .collect();
            let mut message = l[k + 10..].to_string();
            for x in &more {
                message = format!("{message} {}", x[tag.len()..].trim());
            }
            let span = num_after(&message, "input line ").map(|n| (None, n));
            let raw = std::iter::once(l).chain(more).map(String::from).collect();
            out.push(Diag {
                level: "warning",
                code: code(&message),
                message,
                span,
                raw,
            });
        } else if l.starts_with("Overfull \\") || l.starts_with("Underfull \\") {
            let span = num_after(l, "at lines ")
                .or_else(|| num_after(l, "at line "))
                .map(|n| (None, n));
            out.push(Diag {
                level: "warning",
                code: code(l),
                message: l.to_string(),
                span,
                raw: vec![l.to_string()],
            });
        }
    }
    out
}

/// A log line asking for another engine run.
pub fn rerun_request(log: &str) -> Option<&str> {
    log.lines().find(|l| {
        [
            "Rerun to get",
            "Please rerun",
            "Please (re)run",
            "Rerun LaTeX",
        ]
        .iter()
        .any(|p| l.contains(p))
    })
}

/// Files a BibTeX or Biber run read, and those BibTeX could not open, from its `.blg`.
pub fn blg(blg: &str) -> (Vec<&str>, Vec<Missing>) {
    let (mut reads, mut miss) = (Vec::new(), Vec::new());
    for l in blg.lines() {
        if let Some(f) = l
            .strip_prefix("Database file #")
            .and_then(|r| r.split_once(": ").map(|(_, f)| f))
            .or_else(|| l.strip_prefix("The style file: "))
            .or_else(|| quoted(l, "Found BibTeX data source '", "'"))
        {
            reads.push(f);
        } else if let Some(f) = l
            .strip_prefix("I couldn't open database file ")
            .or_else(|| l.strip_prefix("I couldn't open style file "))
        {
            miss.push(Missing {
                kind: Kind::File,
                name: f.to_string(),
                fatal: true,
                substitute: None,
                raw: vec![l.to_string()],
            });
        }
    }
    (reads, miss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn got(log: &str) -> Vec<(Kind, &str, bool)> {
        missing(log)
            .into_iter()
            .map(|m| (m.kind, &*m.name.leak(), m.fatal))
            .collect()
    }

    #[test]
    fn missing_from_real_log() {
        use Kind::*;
        assert_eq!(
            got(include_str!("../tests/fixtures/missing.log")),
            [
                (File, "zzmissing.sty", true),
                (File, "zznofile.tex", true),
                (FontFile, "zzfont.tfm", true),
                (FontFile, "zzfontb.tfm", true),
                (FontOutline, "cmr12", false),
                (FontOutline, "tcrm1200", true),
                (FontName, "TeX Gyre Termes", true),
                (FontFile, "texgyretermes-regular.otf", true),
                (FontFile, "texgyrepagella-regular.otf", true),
            ]
        );
        let pk = &missing(include_str!("../tests/fixtures/missing.log"))[4];
        assert_eq!(pk.substitute, Some("pk"));
    }

    #[test]
    fn file_names_follow_message_context() {
        // graphicx: no extension, and it is not a TeX input.
        assert_eq!(
            got("! LaTeX Error: File `fig' not found."),
            [(Kind::File, "fig", true)]
        );
        assert_eq!(
            got("./a.tex:3: LaTeX Error: File `x.cls' not found."),
            [(Kind::File, "x.cls", true)]
        );
        assert!(got("LaTeX Warning: File `fig' not found on input line 3.").is_empty());
        // A dot in a directory is not an extension.
        assert_eq!(
            got("! I can't find file `sec/v1.2/intro'."),
            [(Kind::File, "sec/v1.2/intro.tex", true)]
        );
        assert_eq!(
            got("! I can't find file `./ch1'."),
            [(Kind::File, "./ch1.tex", true)]
        );
    }

    #[test]
    fn loader_error() {
        let out = "pdflatex: error while loading shared libraries: libfoo.so.1: cannot open shared object file\n";
        let m = missing_library(out).unwrap();
        assert_eq!((m.kind, m.name.as_str()), (Kind::Library, "libfoo.so.1"));
        assert!(missing_library("fine\n").is_none());
    }

    #[test]
    fn fonts_from_lualatex_log() {
        let got = log_fonts(include_str!("../tests/fixtures/lualatex-tail.log"));
        let names: Vec<_> = got.iter().map(|p| p.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, ["lmroman10-regular.otf", "NotoSerifCJK-Regular.ttc"]);
    }

    #[test]
    fn fonts_from_xdv() {
        let got = xdv_fonts(include_bytes!("../tests/fixtures/xecjk.xdv"));
        let names: Vec<_> = got.iter().map(|p| p.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, ["FandolSong-Regular.otf", "lmroman10-regular.otf"]);
    }

    #[test]
    fn inputs_from_fls() {
        let got = fls_paths(include_str!("../tests/fixtures/demo.fls"), "INPUT");
        assert!(got.contains(Path::new("/home/user/demo/main.tex")));
        assert!(got.iter().all(|p| p.is_absolute()));
        assert!(
            fls_paths("PWD /w\nOUTPUT ./main.aux\n", "OUTPUT").contains(Path::new("/w/main.aux"))
        );
    }

    #[test]
    fn diagnostics_from_log() {
        let log = "./sec/a.tex:12: Undefined control sequence.\nl.12 \\foo\n       \nnext\n\
LaTeX Warning: Reference `x' on page 1 undefined on input line 7.\n\
Package natbib Warning: Citation `k' on page 1 undefined on input line 9.\n\
Package hyperref Warning: Token not allowed\n(hyperref)                removing `math shift' on input line 3.\n\
Underfull \\hbox (badness 10000) in paragraph at lines 20--21\n";
        let d = diagnostics(log);
        let got: Vec<_> = d
            .iter()
            .map(|d| (d.level, d.code, d.span.clone()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "error",
                    Some("undefined-control-sequence"),
                    Some((Some("./sec/a.tex".into()), 12))
                ),
                ("warning", Some("undefined-reference"), Some((None, 7))),
                ("warning", Some("undefined-citation"), Some((None, 9))),
                ("warning", None, Some((None, 3))),
                ("warning", Some("underfull-box"), Some((None, 20))),
            ]
        );
        assert_eq!(d[0].raw.len(), 3);
        assert_eq!(
            d[3].message,
            "Token not allowed removing `math shift' on input line 3."
        );
        assert_eq!(
            rerun_request(
                "LaTeX Warning: Label(s) may have changed. Rerun to get cross-references right."
            ),
            Some("LaTeX Warning: Label(s) may have changed. Rerun to get cross-references right.")
        );
    }
}
