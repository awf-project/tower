# Workspace configuration

Tower reads optional project settings from `<workspace>/.tower/config.toml`.
The parser is strict: malformed TOML and unknown keys fail startup rather than being ignored.
An absent file uses defaults.

## Initialize without overwriting files

```bash
tower init
```

Initialization creates `.tower/config.toml` and `.towerignore` only when the corresponding file
does not already exist. It preserves existing bytes and does not create an index-completion marker
or start formatters. If `.towerignore` already exists, initialization retains the established
refusal to overwrite it; if only the config exists, Tower creates the missing ignore file and
leaves the config unchanged.

## Canonical sections

Native sidecars are controlled by `[extensions]`:

```toml
[extensions]
request_timeout_secs = 30
disabled = ["hello"]
```

Host-owned formatters are configured under `[formatter.tools.<id>]`:

```toml
[formatter.tools.rustfmt]
argv = ["rustfmt", "--edition", "2021"]
extensions = ["rs"]

[formatter.tools.php-cs-fixer]
mode = "inplace"
argv = ["php-cs-fixer", "fix", "{path}"]
extensions = ["php"]
```

Formatter modes are `filter` (the default; stdin to stdout) and `inplace` (the host-managed
temporary file is passed to the command). Use `{path}` where the command needs the file path.
The generated configuration includes the standard formatter mappings; edit or remove them for a
project's toolchain.

Linters are configured per language under `[lint.<language>]`. `timeout_secs` is an optional
positive integer and defaults to `2` when omitted. It is the maximum time allowed for each lint
command, including commands used to prepare or verify a lint fix:

```toml
[lint.rust]
command = "cargo"
args = ["clippy", "--message-format=json"]
timeout_secs = 10
extensions = ["rs"]
format = "rustc-json"
target = "none"
```

Zero, negative, fractional, non-integer, and unrepresentable values fail configuration loading.
Tower derives the lint-specific host budget as
`max([extensions].request_timeout_secs, 2 * max(resolved lint timeout_secs) + 1)` seconds across
configured lint entries. This allows both fix passes and response handling; omitted entries use
the two-second default. The global extension deadline and unrelated extensions are unchanged.
Set an external MCP client's request deadline above this budget, with room for daemon preparation
and transport. A workspace-wide lint request can run commands for several files and may need a
larger client deadline.

The daemon has separate connection and preparation deadlines. The connection deadline only covers
locating or spawning the workspace daemon; it does not cancel preparation already owned by that
daemon. The preparation deadline bounds a workspace-dependent request while reconciliation or
capability catch-up is still running.

```toml
[daemon]
idle_timeout_secs = 30
connection_timeout_secs = 10
preparation_timeout_secs = 10
```

The built-in `ast` and `lsp` sidecars already have readiness families. Additional sidecars can be
mapped by manifest name to `ast` or `lsp`:

```toml
[extensions.readiness]
my_ast_extension = "ast"
```

`readiness_controls` is for an extension that implements a non-exhaustive public control callable
while catch-up is in progress. It cannot be configured for the built-in `ast` or `lsp` sidecars.

Other supported top-level sections configure language servers, linters, and debugging. See the
relevant reference pages and generated comments for their fields. Configuration is strict: unknown
keys and zero-valued daemon deadlines fail startup.

## Migrate legacy configuration manually

**BREAKING:** the old top-level `[plugins]` schema is no longer accepted. This includes an empty
table, its `disabled` setting, formatter entries, and files mixing legacy and canonical sections. Startup
fails before the engine or any sidecar starts, and the file is left unchanged.

Replace extension settings with `[extensions]` (including `disabled`) and formatter entries with
`[formatter.tools.<id>]`. Do not rely on automatic rewriting: Tower intentionally requires the
user to review and edit the configuration. After replacing the legacy sections, rerun the daemon;
malformed TOML and unknown keys remain errors.

## Startup and recovery

The daemon acquires workspace lifetime ownership and binds its local transport before opening the
index or preparing extensions. This lets `tower mcp` initialize promptly while preparation continues;
a client timeout does not release daemon ownership or start a competing Sled owner. Use
`tower status` to inspect the current generation and readiness of the workspace, AST, and LSP
families.

Workspace-dependent calls are gated independently. During preparation they either wait up to
`preparation_timeout_secs` or return structured readiness errors. A configured capability that fails
is reported as degraded or failed; an absent or disabled capability is reported as unavailable.

The daemon reconciles the eligible filesystem inventory with the persisted index on every startup,
even if a previous scan-completion marker exists. It uses `.towerignore` and the same eligibility
rules as `tower_reindex`, preserves surviving identities, and reports added, removed, and unchanged
counts on `stderr`. A failed reconciliation does not expose a partial service; the next startup
retries the repair.

See [`.towerignore`](../towerignore.md) for visibility rules and
[the MCP reference](../mcp-tools.md) for the explicit reindex operation.
