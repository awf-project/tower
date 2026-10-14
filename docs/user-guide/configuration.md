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

Other supported top-level sections configure language servers, linters, debugging, and daemon
behavior. See the relevant reference pages and the generated comments for their fields.

## Migrate legacy configuration manually

**BREAKING:** the old top-level `[plugins]` schema is no longer accepted. This includes an empty
table, its `disabled` setting, formatter entries, and files mixing legacy and canonical sections. Startup
fails before the engine or any sidecar starts, and the file is left unchanged.

Replace extension settings with `[extensions]` (including `disabled`) and formatter entries with
`[formatter.tools.<id>]`. Do not rely on automatic rewriting: Tower intentionally requires the
user to review and edit the configuration. After replacing the legacy sections, rerun the daemon;
malformed TOML and unknown keys remain errors.

## Startup recovery

The daemon reconciles the eligible filesystem inventory with the persisted index on every startup,
even if a previous scan-completion marker exists. It uses `.towerignore` and the same eligibility
rules as `tower_reindex`, preserves surviving identities, and reports added, removed, and unchanged
counts on `stderr`. A failed reconciliation does not expose a partial service; the next startup
retries the repair.

See [`.towerignore`](../towerignore.md) for visibility rules and
[the MCP reference](../mcp-tools.md) for the explicit reindex operation.
