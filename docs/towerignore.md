# `.towerignore` — the index ignore source

> Status: implemented and stable. The initial scan, forced reindex, and the live
> watcher all honour `.towerignore` consistently (verified clean). One unrelated
> footgun remains in the `tower init` CLI dispatch (see
> [Known issue](#known-issue-cli-dispatch)).

tower's file index is **authoritative and independent of git**. The walker no
longer consults `.gitignore`, `.git/info/exclude`, or the global gitignore. The
**sole** source of ignore rules is a `.towerignore` file at the workspace root.

This decouples what the agent/LLM can see from what git tracks: build output and
secrets you keep out of the index are governed by tower, not inherited from
whatever happens to be in `.gitignore`.

---

## Behaviour

- **Sole source.** Only `.towerignore` is read. Git ignore files are ignored
  entirely (`git_ignore`, `git_exclude`, `git_global` are all off).
- **Same syntax as `.gitignore`.** Glob patterns, directory patterns (`logs/`),
  prefix patterns (`logs/*`), and `!` negation to re-include a previously excluded
  path all work identically.
- **Cascading per-directory (scan & reindex).** A `.towerignore` may live in any
  subdirectory and applies to that subtree, exactly like nested `.gitignore` files —
  during the initial scan and `tower_reindex`. The **live watcher** consults the root
  file only; see [Watcher limitation](#watcher-limitation--root-towerignore-only).
- **Hidden files and `.git/` are always skipped**, independent of any
  `.towerignore` content. Symlinks are never followed.
- **Honored outside a git repo.** Rules apply even when the workspace is not a git
  repository.
- **The live watcher honours `.towerignore` and the hidden-file rule**, consistent
  with the initial scan. A file created or modified *after* startup that matches an
  ignore pattern (or is hidden) is **never indexed** — a secret dropped in while the
  engine runs stays out of the index, exactly as it would on a cold scan. If an
  existing indexed file *becomes* ignored (its path starts matching a pattern), the
  watcher **drops it from the index** on the next event for that path.

### When `.towerignore` is absent

tower **does not fail**. It prints a one-time warning to stderr and indexes
**everything except `.git/` and hidden files**:

```
tower: warning — no .towerignore found at <workspace>; indexing all non-hidden
files. Run `tower init` to scaffold one.
```

The warning fires **once per boot** (at the public entry point, not inside the
shared walker), so every startup against a workspace with no `.towerignore`
re-emits it — a persistent reminder, not a one-shot suppressed after the first run.

---

## ⚠️ BREAKING CHANGE

**Existing workspaces that relied on `.gitignore` to keep files out of the index
will see their index grow.** Anything previously excluded only by `.gitignore`
(for example `target/`, `node_modules/`, `dist/`, build caches, and crucially any
secrets) is now indexed and therefore visible to the agent/LLM — until you add a
`.towerignore`.

**Migration:** run `tower init` at the workspace root to scaffold a default
`.towerignore`, then add any project-specific patterns. If you previously curated
a `.gitignore` for index hygiene, copy the relevant exclusion patterns across —
they use the same syntax.

> Secrets first. Re-add patterns for `.env`, key material, and credential
> directories before running the engine against an untrusted agent, since these
> are now indexed by default when no `.towerignore` is present.

---

## Scaffolding with `tower init`

```bash
tower init
```

Creates `.towerignore` at the resolved workspace root. It **refuses to overwrite**
an existing file (exits non-zero, leaving your edits untouched).

### Default template

```gitignore
# tower index ignore — authoritative, independent of .gitignore.
# Same syntax as .gitignore. Negate with ! to re-include a path.

# Build artifacts
target/
node_modules/
dist/
vendor/

# Secrets (must never be fed to the agent/LLM)
*.key
*.pem
.env
.env.*
secrets/
```

---

## Examples

Exclude a directory's contents but keep one file:

```gitignore
logs/*
!logs/keep.log
```

Per-directory override — a `.towerignore` inside `crates/ast/` applies only to
that subtree:

```
workspace/
├── .towerignore          # root rules (cascade everywhere)
└── crates/ast/
    └── .towerignore      # additional rules scoped to crates/ast/
```

---

## Watcher limitation — root `.towerignore` only

The **live watcher** evaluates events against the **root** `<workspace>/.towerignore`
only. Nested per-directory `.towerignore` files are **honoured by the initial scan
and by `tower_reindex`** (the `ignore` crate's `WalkBuilder` cascades them), but the
watcher's runtime filter does **not** consult them. A file matched solely by a nested
`.towerignore` is therefore excluded on a scan/reindex but could be (re)indexed by a
live event until the next reindex.

**Why this is safe for the default template.** The default `.towerignore` secret
globs are **un-anchored** — `*.key`, `*.pem`, and `secrets/` have no leading `/`, so
the `ignore` crate matches them at **any depth** from the single root file. A secret
named `crates/ast/secrets/foo` or `deep/nested/id_rsa.pem` is caught by the root rule
alone; no nested file is required to protect it. The limitation only bites
project-specific rules that exist *exclusively* in a nested `.towerignore` and have no
equivalent at the root — and never the built-in secret protection.

> Recommendation: keep secret patterns at the **root** `.towerignore`. Reserve nested
> files for scoped, non-security exclusions (and run `tower_reindex` after adding one
> so the live watcher's view matches).

---

## Known issue (CLI dispatch)

`tower init` is currently detected by scanning argv for the literal token `init`
anywhere after the program name. A flag value or path equal to `init` (for
example `tower --workspace-dir init`) spuriously triggers scaffolding instead of
the intended command. This is a pre-existing loose-arg-scanning pattern, not a
regression of the ignore walker, but it must be resolved (proper positional
subcommand parsing) before `.towerignore` is declared stable.

---

## Implementation notes

- Walker: `crates/core_engine/src/adapters/fs/scan.rs` — `workspace_walker()`
  configures the `ignore` crate's `WalkBuilder` (git off, custom ignore filename
  on, cascading nested files). Shared by the initial scan and the forced
  `tower_reindex` path so both apply identical rules.
- Live watcher filter: the `NotifyWatcherAdapter` applies the same ignore + hidden
  rules at event time using the **root** `.towerignore` only (no nested-file
  cascade — see [Watcher limitation](#watcher-limitation--root-towerignore-only)).
  Create/modify events that match are skipped; a path that newly matches is removed
  from the index.
- Constants: `TOWERIGNORE_FILE_NAME` (`.towerignore`) and `DEFAULT_TOWERIGNORE`
  (the template above).
- The absent-file warning (`warn_if_towerignore_absent`) lives in the public entry
  points, not in the shared walker, so it is emitted once per boot.
