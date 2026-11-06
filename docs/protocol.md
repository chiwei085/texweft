# TeX Build Protocol — design record

A record of the protocol texweft implements and why it took this shape. It
describes; it does not bind. When the code and this file disagree, the code is
current and this file is out of date.

TeXweft emits JSON Lines so editors, CI, and tools such as texup can consume
build results without parsing TeX logs.

```sh
texweft build main.tex [options]
texweft watch main.tex [options] [--poll]
texweft clean main.tex [--outdir DIR] [--lock-timeout SECS]

options: --engine pdflatex|xelatex|lualatex  --message-format=json|human  --force
         --outdir DIR  --step-timeout SECS  --lock-timeout SECS
```

## Output directory

Outputs go to `build/` by default, never next to the sources; `--outdir .`
restores the in-source layout. A relative `--outdir` is relative to the main
file's directory, not to the caller's working directory, so the same project
builds into the same place from anywhere. `--outdir` is a texweft argument
only: texweft never edits the project (no `latexmkrc`), so the sources stay
what Overleaf or any other toolchain expects, and `build/` need not be uploaded.

- Every tool runs from the main file's directory, so `\input{sections/a}`,
  `\includegraphics{figures/x}`, and `\graphicspath` mean what they mean
  elsewhere. The engine gets `-output-directory`; web2c engines read `.aux`,
  `.toc`, `.bbl`, and files the document `\openout`s back from there.
- `\include{ch/one}` writes `ch/one.aux` into the output directory, and TeX
  cannot create `ch/`. Before each engine run texweft creates the directories
  of include targets the `.aux` tree names. On a first build the `.aux` does not
  name them yet, so the run stops at ``I can't write on file `ch/one.aux'``;
  texweft creates the directory only when that file is an include target the
  `.aux` now names and the path is plain relative (no `..`, not absolute),
  then reruns with `rerun.kind: "output-dirs-created"` (`dirs`). These runs do
  not count toward the engine run limit; at most 64 directories per build. The
  source tree is never mirrored.
- BibTeX runs in the output directory, next to the `.aux` it reads, with the
  source directory prepended to `BIBINPUTS` and `BSTINPUTS`. Biber gets
  `--output-directory` (untested: no biber where this was written). xdvipdfmx
  runs from the source directory, since images in the `.xdv` are named relative
  to it.
- SyncTeX records absolute source paths, so viewers resolve them from `build/`.
- The state file `<job>.texweft.json` and the lock `.<job>.texweft.lock` live in
  the output directory.

Equivalence was checked on unmodified documents (cross-references, toc/lof/lot,
`\include` in nested subdirectories, `\input`, `\graphicspath`, BibTeX, TikZ,
amsthm, the `comment` package's write-and-read-back, a custom
`\IfFileExists{\jobname.cnt}` round trip, and LuaLaTeX): with
`SOURCE_DATE_EPOCH` fixed, texweft's `build/main.pdf` and `latexmk -pdf`'s
in-source `main.pdf` were byte-identical apart from `/ID`, which pdfTeX derives
from the output path.

`watch` builds, then rebuilds whenever an input's content changes. Its stdout
is a sequence of builds, each delimited by `build-started` … `build-finished`
and followed by a `watching` message:

```json
{"v":0,"type":"watching","inputs":17,"dirs":9,"backend":"native"}
```

- What it watches: every `dep-info` input, the main file, each `ls-R` in
  `$TEXMFDBS` (tlmgr rewrites it on install, so a build that ended `missing`
  reruns once the caller installs the package), and where each missing file or
  program would appear (`PATH` directories for programs).
- Events are hints; content decides. The watch keeps a dirty state, not a
  queue: a change counts only when a content hash differs from what the last
  build used, so `touch`, identical rewrites, A→B→A edits, and the build's own
  outputs never rebuild, however many events arrived.
- Changes during a build never cancel it (a killed engine can leave a half-written
  `.aux`); they are re-checked by content once it finishes. Some changes produce
  no event the watch can see: an input modified during the build (recorded
  without a hash), an input in a directory watched only after the build, an
  awaited file created before its directory was watched. These trigger an
  immediate rebuild, at most twice in a row per path, so a file the build keeps
  rewriting cannot loop.
- Events settle for 100 ms (at most 1 s) before a build, so a burst of writes
  from one save is one build.
- Native events (inotify, FSEvents, ReadDirectoryChangesW) by default; polling
  every 500 ms with `--poll` (NFS, WSL `/mnt/c`, container mounts), or
  automatically when the OS watch limit is reached.
- An idle watch checks every 500 ms whether its stdout pipe still has a reader,
  and exits with 141 when it does not.

`clean` removes only what texweft manages in the output directory: the `.fls`
`OUTPUT` files inside it, the PDF, `.bbl`, `.blg`, and the state file, then the
subdirectories that held only those (`rmdir`, so anything else keeps them). The
output directory and the lock file stay. It prints removed paths to stderr and
emits no protocol messages.

Process lifecycle, for `build` and `watch` alike:

- Each tool runs in its own process group with stdin closed, so it can neither
  wait for terminal input nor survive texweft. Termination sends SIGTERM to the
  whole group (the engine and helpers such as mktexpk), then SIGKILL after 2 s,
  and reaps the tool. A group that outlives a tool's normal exit is killed too,
  since a stray helper would hold the tool's output pipes open.
- On SIGINT, SIGTERM, or SIGHUP texweft stops starting tools and writing
  messages, stops the running tool's group, and exits with 128 + signal: 130,
  143, 129. A closed stdout exits with 141. No state file is left by a build
  that did not finish, so the next build reruns.
- `--step-timeout SECS` stops any single step that runs longer and reports a
  `diagnostic` with `code: "timeout"`; the build is `failed`. There is no
  default timeout: large documents legitimately take minutes.
- Windows: only the tool itself is killed; its helpers would need a Job Object.

Builds that share an output namespace (output directory and jobname)
would overwrite each other's `.aux` and logs. Every texweft command that writes
there (`build`, `watch`, `clean`) takes an OS advisory lock on
`.<job>.texweft.lock` in the output directory first; the OS releases it if texweft dies, so a crash
never leaves a stale lock. While another texweft holds it:

```json
{"v":0,"type":"lock-wait","lock":"/w/paper/build/.main.texweft.lock","holder_pid":4242}
```

`build` and `clean` wait at most `--lock-timeout` (default 30 s), then end with
`outcome: "busy"` (`clean`: exit 1, nothing removed). `watch` waits as long as
it takes, then decides from the state file and content whether to build: the
other build may have used a different engine or other inputs, so having waited
is not evidence that nothing is left to do. The lock file stays after `clean`,
because deleting it would let a waiter lock an unlinked file. Tools run outside
texweft (`pdflatex main.tex` by hand) do not take the lock.

Without `--message-format=json`, a short human summary goes to stderr. The engine
runs in the main file's directory with `-interaction=nonstopmode -file-line-error
-recorder -synctex=1 -output-directory=<outdir>` and `max_print_line=10000`; XeLaTeX runs with `-no-pdf`,
followed by an `xdvipdfmx` step that does not count toward the engine run limit.

Each line is one message with `"v": 0` and `"type"`. All paths are absolute.
stdout contains only protocol messages; human-readable and raw tool output goes
to stderr or files. Every build ends with `build-finished`.

## Messages

| `type` | Emitted |
|---|---|
| `build-started` | Once at the start |
| `step-started` | Before each engine or external tool invocation |
| `diagnostic` | For errors and warnings parsed from logs |
| `missing` | For missing files or fonts |
| `unsupported` | When the document requires an unsupported workflow |
| `step-finished` | After each step |
| `artifact` | For build artifacts |
| `dep-info` | Once before completion, listing inputs actually read |
| `build-finished` | Always the last message |

### build-started

```json
{"v":0,"type":"build-started","main":"/w/paper/main.tex","engine":"pdflatex","outdir":"/w/paper/build","tex_root":"/home/u/.texup/2023"}
```

`tex_root` is the canonical path to the TeX installation in use. A change
invalidates all fingerprints; see `toolchain-changed`.

### step-started

```json
{"v":0,"type":"step-started","step":3,"tool":"pdflatex","rerun":{"kind":"inputs-changed","files":[{"path":"/w/paper/main.aux","was":"9f2c…","now":"41ab…"}]}}
```

`step` increases within the build. `rerun` explains why the step runs:

| `rerun.kind` | Meaning | Additional fields |
|---|---|---|
| `no-previous-run` | No usable previous state | — |
| `inputs-changed` | Input content hashes changed | `files[]`: `path`, `was`, `now`; `was: null` means a new input |
| `toolchain-changed` | `tex_root` or engine changed | `was`, `now` |
| `log-requested` | The log requested a rerun, e.g. `Rerun to get cross-references right` | `message` |
| `forced` | The caller requested a rebuild, e.g. `--force` | — |
| `output-dirs-created` | The previous run stopped because an `\include` target's output directory did not exist | `dirs` |

Within a build, an engine run reruns when it changed a file under the output
directory that the next run reads: a file it both read and wrote (`.aux`, `.toc`),
or one it newly created. Logs, PDFs, and caches outside the output directory
do not count. A bibliography step runs when its control data (`\citation`,
`\bibdata`, `\bibstyle` lines of the `.aux` tree, or the `.bcf`) or a database it
read changed, and its new `.bbl` triggers an engine rerun.

Across builds, the driver keeps `<job>.texweft.json` in the output directory,
written only after `converged`. An input whose mtime is later than the build's
start is recorded without a hash, so a file saved mid-build triggers the next
build instead of being taken as up to date. When the toolchain, every recorded input, and the
PDF are unchanged, nothing runs: `steps` is 0, artifacts are reported with
`fresh: false`, and `preview` is the PDF.

### diagnostic

```json
{"v":0,"type":"diagnostic","step":1,"level":"error","code":"undefined-control-sequence","message":"Undefined control sequence.","span":{"file":"/w/paper/sec/intro.tex","line":42},"raw":["./sec/intro.tex:42: Undefined control sequence.","l.42 \\foo"]}
```

- `level`: `error` or `warning`.
- `code`: optional; currently `timeout` (a step exceeded `--step-timeout`), `undefined-control-sequence`, `undefined-reference`,
  `undefined-citation`, `overfull-box`, and `underfull-box`. Omit if unrecognized.
- `span`: optional. `file` comes from `-file-line-error` and is `null` when the
  log names only a line (`on input line 7`, overfull boxes).
- `diagnostic`, `missing` (from the engine log), and `unsupported` describe the
  last engine run only. Warnings of runs that led to a rerun, such as undefined
  references before the `.aux` existed, are transient and not reported. Errors
  always end the build, so they always come from the last run.
- `raw`: always includes the original log lines as a fallback for callers.

### missing

```json
{"v":0,"type":"missing","step":1,"kind":"file","name":"siunitx.sty","fatal":true,"raw":["! LaTeX Error: File `siunitx.sty' not found."]}
{"v":0,"type":"missing","step":1,"kind":"font-outline","name":"ecrm1200","fatal":false,"substitute":"pk","raw":["</home/u/.texup/2023/texmf-var/fonts/pk/ljfour/jknappen/ec/ecrm1200.600pk>"]}
{"v":0,"type":"missing","step":1,"kind":"font-outline","name":"tcrm1200","fatal":true,"raw":["!pdfTeX error: pdflatex (file tcrm1200): Font tcrm1200 at 600 not found"]}
{"v":0,"type":"missing","step":1,"kind":"font-name","name":"TeX Gyre Termes","fatal":true,"raw":["! Font \\TU/TeXGyreTermes(0)/m/n/10=\"TeX Gyre Termes\" at 10.0pt not loadable: ..."]}
```

- `kind`:
  - `file`: a TeX input, named as the log names it. LaTeX's
    `` File `x.sty' not found `` already carries the extension. Only TeX's
    `` I can't find file `x' `` gets `.tex` appended, and only when the base
    name has no extension. Other names stay verbatim, e.g. graphicx's
    `` File `fig' not found ``.
  - `font-file`: an exact font file name: `.tfm` from `Font \x=name not loadable`,
    `file.otf` from `[file.otf]`.
  - `font-name`: a fontspec family name; the caller resolves it to a file.
  - `program`: a tool that is not installed (`biber`, `xdvipdfmx`, or the engine
    itself, then with `step: 0`).
  - `library`: a shared library the dynamic loader could not find, from the
    tool's own output (`error while loading shared libraries: libfoo.so.1`). It
    is a system package, not TeX Live; the caller reports it.
  - `font-outline`: the TeX font `name` (no extension) has no outline font on the
    engine's map. The driver observed only that; it does not claim which file is
    missing. `name.pfb` may exist (`cmr12` in amsfonts), may exist under another
    name (`tcrm1200` → cm-super's `sfrm1200.pfb`), may be installed but absent
    from the map, or may not exist at all (METAFONT-only fonts). The caller maps it.
- `fatal: false` with `substitute` reports a silent fallback: the build can
  succeed with altered output, such as a PK bitmap (`substitute: "pk"`) replacing
  a Type 1 font. `build-finished.outcome` is not `missing` in this case.

The driver reports missing dependencies; the caller installs them and invokes
it again. There are no install hooks because an unmodified engine cannot resume
in place. After installation, `rerun` must identify newly available inputs without
requiring a force flag. Installing only an outline font changes no document input;
the rerun comes from `pdftex.map`, which updmap rewrites and pdfTeX's `.fls`
records as an input (verified with TeX Live 2023).

### unsupported

```json
{"v":0,"type":"unsupported","step":1,"flow":"makeindex","evidence":"/w/paper/main.idx"}
```

An unsupported workflow sets `build-finished.outcome` to `unsupported`, even if
a PDF was produced. Detected: `makeindex` (`.idx` written), `glossaries`
(`.glo` or `.acn` written), and `shell-escape` (`runsystem(` in the log, unless
disabled). False positives were preferred to missed workflows.

### step-finished

```json
{"v":0,"type":"step-finished","step":3,"tool":"pdflatex","exit_code":0,"duration_ms":812,"log":"/w/paper/main.log"}
```

The driver deletes a step's log (`.log`, `.fls`, `.blg`) before running it, so a
tool that dies before writing one never reports the previous run's messages;
`log` is then `null` and `exit_code` is `null` if the tool could not start.

### artifact

```json
{"v":0,"type":"artifact","kind":"pdf","path":"/w/paper/main.pdf","step":5,"fresh":true}
```

- `kind`: `pdf`, `synctex`, or `xdv`.
- `fresh`: whether this build wrote the file, determined from its mtime and hash
  recorded at build start. `false` means an artifact from an earlier build.

### dep-info

```json
{"v":0,"type":"dep-info","inputs":[{"path":"/home/u/.texup/2023/texmf-dist/tex/latex/siunitx/siunitx.sty","via":"fls"},{"path":"/w/paper/refs.bib","via":"bibtex"},{"path":"/home/u/.texup/2023/texmf-dist/fonts/opentype/public/tex-gyre/texgyretermes-regular.otf","via":"xdv"}]}
```

`via` identifies the source: `fls`, `log` (LuaTeX `</…otf>` lines), `xdv`
(opcode 252), or `bibtex` / `biber` (bibliography inputs absent from `.fls`).
This message supplies all inputs needed for texup's lockfile.

### Engine run limit

One `texweft build` runs the engine at most 5 times, counting the first run.
BibTeX and Biber steps appear in `steps` but do not count toward the limit, so
`build-finished.steps` can exceed 5. The build stops as soon as no rerun
condition holds. If one still holds after the fifth engine run, the outcome is
`not-converged` with `last_rerun`. Each invocation starts a new count; callers
that install dependencies and retry bound their own loop. 5 follows latexmk's
`$max_repeat`.

### build-finished

```json
{"v":0,"type":"build-finished","outcome":"missing","success":false,"steps":1,"preview":null}
{"v":0,"type":"build-finished","outcome":"failed","success":false,"steps":4,"preview":"/w/paper/main.pdf"}
{"v":0,"type":"build-finished","outcome":"converged","success":true,"steps":5,"preview":"/w/paper/main.pdf"}
```

| `outcome` | `success` | Meaning |
|---|---|---|
| `converged` | true | All steps succeeded and rerun conditions stabilized |
| `missing` | false | A fatal dependency is missing; the caller can install it and retry |
| `failed` | false | An error occurred or a tool exited with a nonzero status |
| `not-converged` | false | The engine run limit was reached; includes `last_rerun`, the final `rerun` object |
| `unsupported` | false | An unsupported workflow was detected |
| `busy` | false | Another texweft held the output lock past `--lock-timeout`; nothing ran |

`preview` is the path to a fresh PDF (or the verified PDF of an up-to-date
build), or `null`, independently of `success`.
The process exits with 0 for `converged` and 1 otherwise; details are in this
message.

## How texup consumes it

texup runs the binary as a subprocess and reads only these messages. Its
retry loop, as designed:

```text
loop {
    msgs = texweft build --message-format=json
    match msgs.build_finished.outcome {
        missing => resolve fatal missing[] through tlpdb;
                   stop if all packages were already installed;
                   otherwise run tlmgr install and retry
        _       => if non-fatal missing[] (font-outline with substitute)
                   maps to packages not yet installed: install and retry;
                   otherwise write the lockfile from dep-info and return
    }
}
```

The division of labour behind it: texweft parses logs, normalizes names, and
detects fallbacks; texup owns installation, lockfiles, and package mapping
(tlpdb lookups, `-dev` ranking, `font-outline` → file such as the ec/tc prefix
rule → cm-super, font-name resolution, anything needing `.map` files).
