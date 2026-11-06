# texweft

**A TeX build driver that tells its caller what happened.** texweft runs
pdfLaTeX, XeLaTeX, or LuaLaTeX with BibTeX or Biber, reruns until
cross-references settle, and reports everything as JSON Lines on stdout, the way
`cargo build --message-format=json` does. Editors and tools read those messages
instead of parsing the `.log` with their own regexes.

What a caller gets that latexmk does not tell it:

- **Structured diagnostics** with file, line, and a stable `code`. A real error
  is never downgraded to a warning.
- **Why each step ran**: no previous run, which inputs changed (with their old
  and new hashes), a log request, or a toolchain change.
- **Missing files, fonts, and programs** as `missing` messages, including silent
  fallbacks such as a PK bitmap substituted for an outline font. The build ends
  with `outcome: missing`, so the caller can install the dependency and run again.
- **An explicit `unsupported` outcome** for flows texweft does not handle
  (makeindex, glossaries, shell escape). texweft never reports success for a PDF
  that is silently incomplete.
- **Whether each artifact is fresh**, and a `preview` PDF that is reported
  independently of `success`.
- **`dep-info`**: every file the build read (including fonts from the `.xdv` or
  the LuaTeX log) and the programs it needs. This is enough to write a lockfile.

texweft leaves the engines unchanged and edits nothing in the project (no
`latexmkrc`), so the sources stay what Overleaf or any other toolchain expects.

## Installation

texweft needs a TeX installation with the engines on `PATH`. Prebuilt binaries
are for x86_64 and aarch64 Linux with glibc 2.35 or later (Ubuntu 22.04,
Debian 12, Fedora 36, or newer). Download the binary for your machine from the
[latest release](https://github.com/chiwei085/texweft/releases/latest):

```bash
mkdir -p ~/.local/bin
curl -fL -o ~/.local/bin/texweft \
  "https://github.com/chiwei085/texweft/releases/latest/download/texweft-$(uname -m)-linux"
chmod +x ~/.local/bin/texweft
texweft --version
```

Make sure `~/.local/bin` is on your `PATH`. To update, rerun the same commands.
Each release also lists a `.sha256` checksum for every binary.

To build from source instead, install Rust 1.89 or later and run:

```bash
cargo install --git https://github.com/chiwei085/texweft
```

Use [texup](https://github.com/chiwei085/texup) if you want a TeX Live installation
pinned per project, with packages installed on demand. texup runs texweft for
its builds.

## Usage

```bash
texweft build main.tex                  # outputs go to build/ next to main.tex
texweft build main.tex --engine xelatex --outdir .
texweft watch main.tex                  # rebuild whenever an input's content changes
texweft clean main.tex                  # remove only what texweft made
```

| Option | Meaning |
| --- | --- |
| `--engine pdflatex\|xelatex\|lualatex` | Engine (default `pdflatex`) |
| `--message-format json\|human` | JSON Lines on stdout, or a short summary on stderr (default) |
| `--outdir DIR` | Output directory, relative to the main file (default `build`) |
| `--force` | Rebuild even if everything is up to date |
| `--step-timeout SECS` | Stop a tool that runs longer than this |
| `--lock-timeout SECS` | How long to wait for another texweft using the same outputs (default 30) |
| `--poll` | `watch` only: poll instead of using native file events |

The exit status is 0 when the outcome is `converged` and 1 otherwise. The
details are in the last message.

## Messages

```console
$ texweft build paper.tex --message-format=json
{"v":0,"type":"build-started","main":"/w/paper.tex","engine":"pdflatex","outdir":"/w/build","tex_root":"/opt/texlive/2025"}
{"v":0,"type":"step-started","step":1,"tool":"pdflatex","rerun":{"kind":"no-previous-run"}}
{"v":0,"type":"step-finished","step":1,"tool":"pdflatex","exit_code":0,"duration_ms":182,"log":"/w/build/paper.log"}
{"v":0,"type":"step-started","step":2,"tool":"bibtex","rerun":{"kind":"no-previous-run"}}
…
{"v":0,"type":"diagnostic","step":4,"level":"warning","code":"overfull-box","message":"Overfull \\hbox (3.2pt too wide) in paragraph at lines 12--14","span":{"file":"/w/paper.tex","line":12},"raw":["…"]}
{"v":0,"type":"artifact","kind":"pdf","path":"/w/build/paper.pdf","step":4,"fresh":true}
{"v":0,"type":"dep-info","inputs":[{"path":"/w/refs.bib","via":"bibtex"},…],"programs":["pdflatex","bibtex"]}
{"v":0,"type":"build-finished","outcome":"converged","success":true,"steps":4,"preview":"/w/build/paper.pdf"}
```

| `outcome` | Meaning |
| --- | --- |
| `converged` | All steps succeeded and nothing asks for another run |
| `failed` | The last engine run reported an error, or a tool failed; the preview may still be complete |
| `missing` | A file, font, or program is missing; install it and run again |
| `unsupported` | The document needs a flow texweft does not support |
| `not-converged` | Still changing after 5 engine runs |
| `busy` | Another texweft held the output lock past `--lock-timeout` |

[docs/protocol.md](docs/protocol.md) is the full record of the messages and
the rules behind them: rerun conditions, how inputs are verified during a
build, the state file, locking, `watch`, and `clean`.

## Performance

Content hashes decide whether something changed, so `touch`, `git checkout`, and
saves with identical bytes do not trigger a rebuild. texweft does not read a file
again while its stat (size, mtime, ctime, inode) is unchanged. A failure caused
by the document is reused, just as a success is. On a simple document the first
engine run usually settles in one pass.

The table compares wall-clock times with latexmk 4.87, both using TeX Live 2025
on Linux. "Up to date" means no file changed since the last build.

| Document | Build from scratch | One-line edit | Up to date |
| --- | --- | --- | --- |
| pdfLaTeX + BibTeX, one page | 194 ms vs 345 ms | 65 ms vs 130 ms | 1 ms vs 51 ms |
| LuaLaTeX + fontspec | 318 ms vs 377 ms | 312 ms vs 370 ms | 2 ms vs 54 ms |
| XeLaTeX + xeCJK | 290 ms vs 340 ms | 275 ms vs 323 ms | 2 ms vs 50 ms |
| `book`, 20 `\include`d chapters + BibTeX | 1.60 s vs 1.84 s | 550 ms vs 610 ms | 2 ms vs 53 ms |

Rerunning a document that fails with no changes takes 2 ms with texweft and
52 ms with latexmk. `watch` starts a build about 100 ms after a save; latexmk
`-pvc` checks every 2 s by default.

## Scope

Supported: pdfLaTeX, XeLaTeX, and LuaLaTeX; BibTeX and Biber; cross-reference
reruns; `\include` into the output directory. Everything else is reported as
`unsupported`, and the build fails. texweft knows nothing about package
managers, so mapping names to packages is the caller's job.

## Development

The contract tests in `tests/contract.rs` run texweft against a real TeX
installation and are skipped when none is found. To use a TeX installation
that is not on `PATH`, point `TEXWEFT_TEST_TEXBIN` at its binaries:

```bash
TEXWEFT_TEST_TEXBIN=~/.cache/texup/tl/2025/bin/x86_64-linux cargo test --release
```
