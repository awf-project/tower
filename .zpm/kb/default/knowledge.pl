% base-schema.pl — Shared AWF knowledge-base schema.
% Loaded automatically from .zpm/kb/default/.

% Dynamic declarations — required by Trealla Prolog so that rules referencing
% these predicates compile even before any facts are asserted at runtime.
:- dynamic(task/3).
:- dynamic(depends_on/2).
:- dynamic(task_complete/1).
:- dynamic(task_file/2).
:- dynamic(component/4).
:- dynamic(stub/3).
:- dynamic(test_file/2).
:- dynamic(source_file/1).
:- dynamic(adr/3).
:- dynamic(workflow_run/3).
:- dynamic(impl_note/3).
:- dynamic(feedback_rule/3).
:- dynamic(imports/2).
:- dynamic(layer/2).
:- dynamic(uses_panic/1).
:- dynamic(coverage/2).
:- dynamic(integrity_violation/2).

% --- Task graph (written by plan, read by implement/fix-errors) ---
% task(Id, Title, Status).  Status ∈ {pending, in_progress, completed, blocked}.
% depends_on(TaskA, TaskB).  TaskA depends on TaskB completing first.
% task_complete(Id).          Written by implement when stubs cleared + tests green.
% incomplete_task(Id).        Derived: task that is pending/in_progress with unresolved stubs.

incomplete_task(Id) :-
    task(Id, _, Status),
    Status \= completed,
    stub(File, _, critical),
    task_file(Id, File).

% --- Component graph (written by plan, read by implement) ---
% component(Id, Name, Path, Type).  Type ∈ {module, file, package, pipeline}.
% pipeline_handled(Path).           Paths managed by pipelines (CHANGELOG, docs/...).
% forbidden_component(Id) :-        Components plan must not include.
%     component(Id, _, Path, _), pipeline_handled(Path).

% forbidden_component/1: Path is bound via component/4, so nonvar-guarded
% pipeline_handled/1 prefix rules fire correctly. Do not call
% pipeline_handled(X) with X unbound — rules will not enumerate.
forbidden_component(Id) :-
    component(Id, _, Path, _),
    pipeline_handled(Path).

% Seed the standard pipeline-handled paths (exact matches)
pipeline_handled('CHANGELOG.md').
pipeline_handled('docs/CHANGELOG.md').
pipeline_handled('README.md').
pipeline_handled('README').
pipeline_handled('CHANGELOG').
pipeline_handled('CLAUDE.md').

% Prefix-based rules (match directories handled by pipelines).
% nonvar/1 guard prevents ZPM 0.1.0 instantiation errors when Path is unbound,
% which would otherwise abort enumeration of the entire predicate (see KNOWN-ISSUES).
% These rules only fire when Path is already bound (the normal use case via forbidden_component/1).
pipeline_handled(Path) :- nonvar(Path), sub_atom(Path, 0, _, _, 'docs/').
pipeline_handled(Path) :- nonvar(Path), sub_atom(Path, 0, _, _, '.specify/memory/').
pipeline_handled(Path) :- nonvar(Path), sub_atom(Path, 0, _, _, 'ADR/').

% --- Stub tracking (written by code, read by implement) ---
% stub(File, Line, Kind).  Kind ∈ {critical, cosmetic, todo, fixme}.
% task_file(TaskId, File).  Links stubs back to tasks.

% --- Test coverage (written by plan/implement, used by audit) ---
% test_file(SourcePath, TestPath).
% covered_by(Source, Test) :- test_file(Source, Test).

% Decision: wrap test_file/2 in catch/3, same reason as current_decision.
% Why: ZPM (Scryer Prolog) raises existence_error when a constitution rule
% evaluates \+ covered_by(File, _) on a fresh KB with zero test_file facts.
% catch/3 intercepts that error and cleanly fails, so the negation succeeds
% ("no test covers this source") and missing_test violations derive correctly.
covered_by(Source, Test) :-
    catch(test_file(Source, Test), _, fail).

% --- ADR (written by plan/spec-generate, read by architecture reviews) ---
% adr(Id, Title, Decision).
% supersedes(NewId, OldId).
% current_decision(Decision) :-
%     adr(Id, _, Decision), \+ supersedes(_, Id).

% Decision: use catch/3 instead of \+/1 for supersedes check.
% Why: ZPM (Scryer Prolog) raises existence_error when \+ calls an
% undefined predicate. catch/3 handles both the undefined-predicate case
% (no supersedes facts yet → succeed) and the normal case (supersedes
% facts exist → \+ evaluates correctly).
% Trade-off: slightly less idiomatic Prolog; necessary for ZPM compatibility.
current_decision(Id, Decision) :-
    adr(Id, _, Decision),
    catch((\+ supersedes(_, Id)), _, true).

% --- Feedback rules (written by feedback workflow, read by review agents) ---
% feedback_rule(Topic, Date, Text).
%   Topic: atom matching CLAUDE.md section slug (e.g. architecture_rules)
%          derived from section name: lowercase, spaces→underscores
%   Date:  ISO 8601 atom 'YYYY-MM-DD' — lexicographic comparison works for
%          date ordering, e.g. Date @>= '2026-04-15' for "last 7 days"
%   Text:  quoted atom with the rule body
% Query "recent feedback" example (caller computes cutoff date):
%   feedback_rule(Topic, Date, Text), Date @>= '2026-04-15'.

% --- Workflow history (written by commit) ---
% workflow_run(Name, Date, Status).  Status ∈ {success, failure, partial}.

% --- LLM-emitted rationale (written by plan/code-review agents, read by audit) ---
% Captured from fenced ```zpm blocks emitted by agents; see
% scripts/zpm/parse-zpm-block.sh.
%
% task_rationale(TaskId, Text).
%   Why this task exists — captured at generate_tasks time.
%   Keyed by TaskId (use upsert: one rationale per task).
%
% task_risk(TaskId, Level, Reason).
%   Level ∈ {low, medium, high}.
%   LLM's self-assessed risk. Keyed by TaskId (use upsert).
%
% review_finding(Issue, File, Line, Severity, Kind).
%   Severity ∈ {info, warning, critical}.
%   Kind: short atom (e.g. 'duplication', 'n_plus_1', 'hard_coded_secret').
%   Multi-valued — many findings per issue. Use assert.
%
% impl_note(File, Symbol, Note).
%   Local implementation decisions captured during the code TDD cycle.
%   Multi-valued — many notes per (File, Symbol). Use assert.

% Convenience rule: high-risk pending tasks (for audit and dashboards).
high_risk_open_task(Id, Title) :-
    task(Id, Title, Status),
    Status \= completed,
    task_risk(Id, high, _).

% --- Generic integrity violation scaffolding ---
% Constitutions assert integrity_violation/2 rules.
% Severity ∈ {error, warning, info}.
% Example (from constitution-go.pl):
%   integrity_violation(layer_inversion, File) :-
%     imports(File, Target),
%     layer(File, domain),
%     layer(Target, infrastructure).

% ─── Project Memory ──────────────────────────────────────────────────────────
:- dynamic(observation/4).
:- dynamic(decision/4).
:- dynamic(convention/3).

% observation(Id, Category, Content, Date)
%   Id: unique atom (auto-generated slug)
%   Category: pattern | convention | quirk | dependency | performance
%   Content: description (single-quoted atom)
%   Date: ISO date atom e.g. '2026-05-24'
%
% decision(Id, What, Why, TradeOff)
%   Architectural/design decisions recorded during workflows
%
% convention(Id, Pattern, Context)
%   Discovered codebase conventions worth preserving

observations_by_category(Cat, Ids) :-
    findall(Id, observation(Id, Cat, _, _), Ids).

decisions_about(Topic, Ids) :-
    findall(Id, (decision(Id, What, _, _), sub_atom(What, _, _, _, Topic)), Ids).
% constitution-rust.pl — Rust project integrity rules (Prolog).
% Loaded automatically from .zpm/kb/ alongside base-schema.pl.
%
% Encodes a machine-checkable subset of Project Constitution v1.0.0 (rust.md).
% Rules produce integrity_violation(Kind, File) facts that audit tooling queries.
%
% ---  4 Principles (verbatim titles + 1-line summaries from rust.md)  --------
%
% P1 — Hexagonal Architecture
%      domain → application → infrastructure → interfaces (inside-out).
%      Domain has ZERO external crate dependencies.
%
% P2 — TDD Methodology
%      RED → GREEN → REFACTOR; 80% minimum coverage; unit tests co-located;
%      integration tests in tests/.
%
% P3 — Rust Idioms
%      No unwrap/expect in library code; thiserror for domain errors;
%      anyhow only at application boundaries; clippy -D warnings; rustfmt.
%
% P4 — Minimal Abstraction
%      No trait without 2+ concrete implementations; prefer composition.
%      (Not modelled: heuristic too weak without AST; no reliable Prolog rule)
%
% ---------------------------------------------------------------------------
%
% FACT SCHEMA (asserted by callers, e.g. via zpm-assert.sh):
%
%   layer(File, Layer)      File belongs to Layer.
%                           Layer ∈ {domain, application, infrastructure, interfaces}
%   imports(File, Target)   File depends on Target (path atom).
%   source_file(File)       File is a Rust source file tracked in the project.
%   coverage(File, Pct)     File has measured test coverage Pct (integer 0-100).
%   uses_unwrap(File)       File calls .unwrap() (P3).
%   uses_expect(File)       File calls .expect("...") (P3).
%   uses_anyhow(File)       File uses the anyhow crate (P3).
%
%   covered_by/2 is defined in base-schema.pl via test_file/2 — do NOT redefine here.
%
% ---------------------------------------------------------------------------

% layer_index/2: numeric order of layers, inner (low) → outer (high).

layer_index(domain,         0).
layer_index(application,    1).
layer_index(infrastructure, 2).
layer_index(interfaces,     3).

% --- Violation kinds modelled in this constitution -------------------------
% Enumeration helper for audit tooling. Without this, querying
% integrity_violation(K, F) with both vars unbound returns [] in ZPM 0.1.0.
violation_kind(layer_inversion).
violation_kind(missing_test).
violation_kind(unwrap_in_library).
violation_kind(anyhow_outside_boundary).
violation_kind(low_coverage).

% --- P1: Layer inversion ----------------------------------------------------
% Violation when a file in an inner layer imports a file in an outer layer.

integrity_violation(layer_inversion, File) :-
    imports(File, Target),
    layer(File, LayerA),
    layer(Target, LayerB),
    layer_index(LayerA, IdxA),
    layer_index(LayerB, IdxB),
    IdxA < IdxB.

% --- P2: Missing test -------------------------------------------------------
% A source_file has no covered_by entry (via base-schema.pl test_file/2).
% Excludes:
%   - main.rs (binary entry point)
%   - lib.rs  (crate root, typically just re-exports)
%   - mod.rs  (module re-export files)
%   - *_test.rs (inline test companion files)
%   - tests/* (integration test directory, Cargo convention)

is_rust_excluded_file('main.rs').
is_rust_excluded_file(File) :- atom_concat(_, '/main.rs', File).
is_rust_excluded_file('lib.rs').
is_rust_excluded_file(File) :- atom_concat(_, '/lib.rs', File).
is_rust_excluded_file('mod.rs').
is_rust_excluded_file(File) :- atom_concat(_, '/mod.rs', File).
is_rust_excluded_file(File) :- atom_concat(_, '_test.rs', File).
is_rust_excluded_file(File) :- atom_concat('tests/', _, File).

integrity_violation(missing_test, File) :-
    source_file(File),
    \+ covered_by(File, _),
    \+ is_rust_excluded_file(File).

% --- P3: unwrap/expect in library code -------------------------------------
% Library layers (domain, application, infrastructure) must not call .unwrap()
% or .expect(); use Result and ? propagation instead.
% The interfaces layer (CLI/API entry points) is exempt.

rust_library_layer(domain).
rust_library_layer(application).
rust_library_layer(infrastructure).

integrity_violation(unwrap_in_library, File) :-
    layer(File, Layer),
    rust_library_layer(Layer),
    uses_unwrap(File).

integrity_violation(unwrap_in_library, File) :-
    layer(File, Layer),
    rust_library_layer(Layer),
    uses_expect(File).

% --- P3: anyhow outside application boundary --------------------------------
% anyhow is for human-readable error messages at the top boundary.
% Using it in domain or application layers undermines typed error contracts.
% interfaces layer is the intended boundary — anyhow allowed there.

anyhow_forbidden_layer(domain).
anyhow_forbidden_layer(application).

integrity_violation(anyhow_outside_boundary, File) :-
    uses_anyhow(File),
    layer(File, Layer),
    anyhow_forbidden_layer(Layer).

% --- P2: Low coverage -------------------------------------------------------
% Any file with a coverage fact below 80% is a violation.

integrity_violation(low_coverage, File) :-
    coverage(File, Pct),
    Pct < 80.

% --- Skipped rules ----------------------------------------------------------
%
% P3 — clippy / rustfmt:
%   Enforced by cargo clippy -D warnings and cargo fmt --check (tooling).
%   Reproducing linting rules in Prolog provides no additional signal.
%
% P4 — minimal abstraction:
%   Heuristic (trait-to-impl ratio, hierarchy depth) needs AST metrics.
%   No reliable static rule.
