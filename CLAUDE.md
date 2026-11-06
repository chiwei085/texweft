# TeXweft — Development guide

TeXweft is a TeX build driver, in the same role as latexmk. Its distinguishing
feature is a machine-readable build protocol: JSON Lines on stdout, modeled on
`cargo build --message-format=json`. [docs/protocol.md](docs/protocol.md) records the protocol's design; it is
not binding. The code is the source of truth; keep the record in step with it.

## Positioning

- What differs from latexmk is **not** content hashing or reading `.fls`:
  latexmk already does both. TeXweft differs in what it reports to callers:
  structured diagnostics, why each step reran, whether an artifact is fresh,
  silent fallbacks, and an explicit `unsupported` outcome.
- Today, editors and tools each parse the `.log` with their own regexes.
  TeXweft should give them one shared layer to rely on instead, the way
  rust-analyzer and bacon rely on cargo's JSON messages.
- ClutTeX already drives the existing engines and handles bibliography, index,
  and glossary tools. TeXweft has to compete on its integration interface and
  how reliably it behaves, not on how many features it lists.

## Principles

- The engines are unchanged. Line wrapping in the log comes from the engine,
  so log parsing still has to cope with it (`max_print_line=10000` helps).
- Missing files: report them and end with `outcome: missing`. The caller
  installs them and invokes TeXweft again. There are no hooks, because the
  engine cannot resume where it stopped.
- When TeXweft does not support a flow (makeindex, glossaries, shell-escape),
  emit `unsupported` and fail the build, even if a PDF exists. The worst
  failure is a successful build whose PDF is incomplete.
- `preview` is reported independently of `success`. A real error must never
  be downgraded to a warning.
- TeXweft knows nothing about package managers or tlpdb. Name mapping that
  needs `.map` files or tlpdb (cm-super `tcrm`→`sfrm`, `*-dev` ranking, font
  name → file) belongs to the caller.
- No `latexmkrc` compatibility.

## Scope v0

- pdfLaTeX, XeLaTeX, LuaLaTeX; BibTeX and Biber; cross-reference reruns.
- Everything else is reported as `unsupported`.

## First consumer: texup

texup (`~/chiwei/texup`) runs the `texweft` binary as a subprocess and reads
only the protocol messages. It does not depend on TeXweft as a crate, and it
downloads the binary for its users. Start by porting texup's verified log,
`.fls`, and `.xdv` handling from `~/chiwei/texup/src/scan.rs`. The engine
quirks behind it are documented in `~/chiwei/texup/CLAUDE.md` (sections
"Compilation", "Missing-file detection", "Silent fallbacks", "XeLaTeX",
"LuaLaTeX"). Its test fixtures in `tests/fixtures/` are real excerpts.
