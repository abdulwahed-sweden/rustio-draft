# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`rustio-draft` is the **setup-time genesis** tool for the
[rustio-admin](https://github.com/abdulwahed-sweden/rustio-admin) ecosystem: it
turns a natural-language brief into a `schema.json` that `rustio-admin import`
then applies deterministically.

The single most important architectural fact — repeated across the crate's docs
and enforced by its structure — is the **AI boundary**:

> **rustio-draft is the ONLY part of the ecosystem that calls an LLM.** It lives
> in its own repo (a separate crate, not in the framework) precisely so the
> runtime library and CLI never gain a network or LLM dependency. rustio-draft
> *authors* a schema; `rustio-admin` *applies* it.

Keep that boundary intact. Do not add code that makes the framework depend on
this crate, and do not add runtime/LLM behavior beyond schema authoring.

Full design & scope lives in the *rustio-admin* repo:
`docs/RUSTIO_DRAFT_SCOPE.md` (referenced as phases F1–F5 throughout the code).

## Commands

```sh
# Build / run the CLI (binary name: rustio-draft)
cargo run -- <subcommand> ...
cargo build

# Tests are fully offline — the Claude API path is unit-tested with mock JSON,
# so no API key is required to run them.
cargo test
cargo test --workspace --all-targets          # what CI runs
cargo test parses_structured_text_block        # a single test by name
cargo test schema::                            # a module's tests

# CI gate (mirror locally before pushing; CI runs with -D warnings):
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Running the tool

```sh
cargo run -- doctor          # validate ANTHROPIC_API_KEY via GET /v1/models (spends no tokens)
cargo run -- new "a blog with posts and comments"     # → writes ./schema.json
cargo run -- refine schema.json "add a published boolean to Post"   # edits in place
cargo run -- serve           # local studio web UI at http://127.0.0.1:8787
```

Only `new`, `refine`, `serve`, and `doctor` need a key; `--help`/`--version` do
not. Shared model knobs (`--model`, `--max-tokens`) and, for `new`/`refine`, the
apply knobs (`--apply`, `--rustio-admin`) live in `GenOpts`/`ApplyOpts` in
`src/main.rs`.

### API key

The key is read by `api_key()` in `src/main.rs` **only** from commands that
actually reach Claude. It loads a gitignored `.env` via `dotenvy` (never
overriding a real `ANTHROPIC_API_KEY` env var), so an exported var always wins.
`.env.example` documents the expected shape.

## Architecture

Five small source files (~1000 lines total). The design centers on one idea:
**the model's output is untrusted and is constrained, then re-validated, against
a closed contract the builder will also enforce.**

- **`src/schema.rs`** — the heart of the crate. Owns three things derived from
  one closed vocabulary (`FIELD_TYPES = text | integer | boolean | timestamp`):
  1. the Rust types the document (de)serializes into (`SchemaDoc` / `SchemaModel`
     / `SchemaField`);
  2. `import_json_schema()` — the JSON Schema handed to the Claude API as a
     **structured output** format, whose `type` field is an `enum` of
     `FIELD_TYPES`, so the model *cannot* emit a type `import` would reject;
  3. `validate()` — re-checks the model's output using the same name/type rules
     the builder applies, collecting *every* problem in one pass.
- **`src/client.rs`** — the one place that talks to an LLM. Calls the Claude
  Messages API over raw HTTP via `reqwest` (there is no official Anthropic Rust
  SDK). `generate()` and `refine()` share `complete()`; `list_models()` backs
  `doctor`. Response parsing (`parse_schema_response`, `parse_model_ids`) is
  deliberately split out of the network path so it can be unit-tested with mock
  JSON. Handles `refusal` / `max_tokens` stop reasons and 401/403 auth errors as
  friendly messages.
- **`src/main.rs`** — the clap CLI (`new` / `refine` / `serve` / `doctor`). The
  `finalize()` helper is the shared write path: it **re-validates before writing
  and refuses to write an invalid schema**, then either prints the next
  `rustio-admin import/plan/commit` steps or (`--apply`) shells out to
  `rustio-admin import` + `plan` and **stops for review — it never runs
  `commit`**.
- **`src/server.rs`** — the local studio (`serve`, F4): an axum app bound to
  `127.0.0.1` only. Same engine behind a small HTTP API (`/api/generate`,
  `/api/refine`, `/api/save`, `/api/field-types`). **The API key stays
  server-side; the browser only ever sees schema JSON.** `save` validates
  exactly as `import` will before writing. The single-page UI is
  `assets/studio.html`, embedded at compile time via `include_str!`.
- **`src/lib.rs`** — thin crate root; re-exports plus `resolve_rustio_admin_bin`
  (`--rustio-admin` flag → `$RUSTIO_ADMIN_BIN` → `rustio-admin` on PATH).

### Two invariants worth guarding

1. **Never write an invalid schema.** Both write paths (`finalize` in the CLI,
   `save` in the server) call `schema::validate` first. `refine` additionally
   retries the model once (`refine_once_valid`) and leaves the file untouched on
   failure.
2. **`FIELD_TYPES` is a hand-mirrored cross-repo contract.** It duplicates the
   builder's list rather than importing it (to avoid a compile-time dependency
   on the framework). A unit test pins the JSON-Schema enum to the const, and the
   rustio-admin monorepo runs a CI drift guard (F5) against its in-tree copy.
   **When the builder adds a field type, this list must be updated to match.**

### Conventions

- Errors use `anyhow` with `.context()`; user-facing messages are actionable
  (they name the file, the fix, or the flag). Match this style.
- Diagnostics go to `eprintln!` (stderr); actual results/success lines to stdout.
- Keep the crate small — five files by design. Add behavior where it belongs
  rather than introducing new layers.
