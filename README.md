# rustio-draft

[![CI](https://github.com/abdulwahed-sweden/rustio-draft/actions/workflows/ci.yml/badge.svg)](https://github.com/abdulwahed-sweden/rustio-draft/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-CLI-orange?logo=rust)
![Schema](https://img.shields.io/badge/output-schema.json-blue)
![AI](https://img.shields.io/badge/AI-setup--time%20only-purple)
![Safety](https://img.shields.io/badge/safety-diff%20protected-green)
![License](https://img.shields.io/badge/license-MIT-green)

**rustio-draft turns a natural-language project brief into a safe `schema.json`
for [rustio-admin](https://github.com/abdulwahed-sweden/rustio-admin).**

It is a setup-time tool only: it may call Claude to draft the schema, but RustIO
Admin applies the result deterministically through `import`, `plan`, and
`commit`. RustIO itself does not depend on AI at runtime.

> **AI drafts. RustIO validates. Diff protects. Human approves.**

---

## What it does

- Turns a natural-language project brief into `schema.json`.
- Lets you **refine** an existing schema safely.
- Provides a localhost **Studio** for visual editing.
- Validates and protects the schema before writing.
- Can run the RustIO Admin `import` + `plan` chain with `--apply`.

## What it does not do

- It is **not** an AI runtime.
- It is **not** an ORM.
- It is **not** a migration engine.
- It does **not** directly change production databases.
- It does **not** commit RustIO Admin plans automatically.
- It does **not** make RustIO Admin depend on Claude or any LLM.

## Why it exists

You can already ask any AI tool to write a schema — but raw AI output is unsafe.
It may:

- produce invalid JSON,
- drop fields,
- remove whole models,
- change field types silently,
- return something that *looks* valid but is semantically destructive.

rustio-draft treats the model's output as **untrusted** and runs it through a
pipeline before anything is written:

```
generate → validate → retry → diff → protect → human review
```

Full design and boundary:
[`docs/RUSTIO_DRAFT_SCOPE.md`](https://github.com/abdulwahed-sweden/rustio-admin/blob/main/docs/RUSTIO_DRAFT_SCOPE.md)
(in the rustio-admin repo).

---

## Quick start

```sh
cp .env.example .env
# paste your ANTHROPIC_API_KEY into .env

cargo run -- doctor
cargo run -- new "a booking system for a salon: clients, staff, appointments"
```

`new` writes `./schema.json`. Then hand it to RustIO Admin:

```sh
rustio-admin import schema.json
rustio-admin plan      # preview (read-only)
rustio-admin commit    # apply atomically
```

Notes on the API key:

- `doctor` checks the key via `GET /v1/models` and **spends no tokens**.
- `.env` is **gitignored** — your key is never committed.
- An exported `ANTHROPIC_API_KEY` also works and **takes precedence** over `.env`.
- Only the commands that call Claude (`new`, `refine`, `serve`, `doctor`) need a
  key; `--help` and `--version` work without one.

## Example output

`new` produces a small, normalised `schema.json`:

```json
{
  "project": "salon",
  "models": [
    {
      "name": "Client",
      "fields": [
        { "name": "full_name", "type": "text" },
        { "name": "email", "type": "text", "unique": true }
      ]
    },
    {
      "name": "Appointment",
      "fields": [
        { "name": "client_id", "type": "integer" },
        { "name": "starts_at", "type": "timestamp" },
        { "name": "status", "type": "text" }
      ]
    }
  ]
}
```

`id` and `created_at` are **not** listed — RustIO Admin adds those implicitly.

---

## Commands

### Generate

```sh
rustio-draft new "a booking system for a salon: clients, staff, appointments"
```

Designs a fresh schema from the brief and writes it (default `./schema.json`).

### Refine

```sh
rustio-draft refine schema.json "add a published boolean to Post"
```

Re-runs the model with the current schema plus your instruction. It **prints a
diff of what changed** before writing, and **refuses destructive changes by
default**:

```sh
rustio-draft refine schema.json "remove the phone field from Client"
# refused: destructive change (nothing written)

rustio-draft refine schema.json "remove the phone field from Client" --allow-destructive
# allowed
```

Refine edits the file in place by default; use `--out other.json` to write
elsewhere.

### Studio

```sh
rustio-draft serve            # → http://127.0.0.1:8787
```

A localhost web UI to draft, edit model/field cards, refine, and download/save.

- **localhost only**,
- the **API key stays server-side**,
- the browser only ever sees schema JSON,
- destructive saves are blocked and offer a **“Save anyway”** confirmation.

### Apply chain

```sh
rustio-draft new "a blog with posts and comments" --apply
```

With `--apply` (run inside a Builder project), rustio-draft:

1. writes the schema,
2. runs `rustio-admin import`,
3. runs `rustio-admin plan`,
4. **stops before commit** — you review the plan and run `rustio-admin commit`
   yourself.

`--apply` also works with `refine`.

## Flags

| Flag                    | Meaning                                                    |
| ----------------------- | --------------------------------------------------------- |
| `--out <path>`          | Output path (default `schema.json`)                       |
| `--model <id>`          | Claude model (default `claude-opus-4-8`)                  |
| `--max-tokens <n>`      | Max response tokens (default `8000`)                      |
| `--force`               | Overwrite an existing output file (`new`)                 |
| `--apply`               | Run `rustio-admin import` + `plan` after writing          |
| `--rustio-admin <path>` | Path to the RustIO Admin binary for `--apply`             |
| `--allow-destructive`   | Allow destructive changes on `refine`                     |

`--allow-destructive` applies to `refine`; in the Studio, destructive saves are
confirmed through the “Save anyway” button instead. `serve` also takes `--port`.

---

## Safety model

rustio-draft assumes the model can be wrong and layers guards so a bad response
can't corrupt your schema.

### 1. Invalid output is not written

- The Claude call uses **structured outputs** — a JSON Schema whose `type` field
  is an `enum` of the builder's field types, so the model *cannot* emit a type
  `import` would reject.
- The schema requires **`minItems: 1`** for both models and fields, so an
  empty/degenerate stub (`{"models":[]}` or a model with no fields) is rejected
  at the API level.
- Every response is then **re-validated locally** with the same name/type rules
  RustIO Admin's `import` uses — including **rejecting duplicate model names and
  duplicate field names** within a model. An invalid schema is never written.

### 2. The model gets corrected feedback

If a response fails validation, rustio-draft re-asks the model with the
**concrete validation errors** included, up to a small retry budget — a targeted
correction rather than a blind retry. This applies to `new`, `refine`, and the
Studio.

### 3. Refine is diff-protected

Before writing a refined schema, rustio-draft computes a **deterministic
semantic diff** between the old and new schema:

- added models,
- removed models,
- added fields,
- removed fields,
- changed field types,
- changed unique flags.

These changes are treated as **destructive** and blocked by default:

- a removed model,
- a removed field,
- a changed field type,
- a relaxed unique flag (`true → false`).

When blocked, the diff is printed, **nothing is written**, and the file is left
byte-for-byte unchanged. Additive changes (new models/fields, tightening a field
to `unique`) are always allowed.

**Without `--allow-destructive`**, a **model-preservation guard** also makes the
model retry if it drops a model — so an *additive* edit recovers when the model
gets over-eager, instead of failing.

**With `--allow-destructive`**, that guard is disabled and the deterministic diff
gate becomes the **single authority**: intentional destructive edits (including
**removing a whole model**) are applied after the diff is printed. This keeps the
flag's behavior predictable — the guard can never override an explicit opt-in.

### 4. Studio save is protected

The Studio's save uses the same guard against the schema currently on disk. A
destructive save returns **`409 Conflict`** with the list of destructive
changes; the UI then offers **“Save anyway”**, which re-sends with the override.

### 5. Network calls are bounded

The Claude API client uses a **connect timeout** and a **total request timeout**,
so a slow or hung upstream can't stall the CLI or the Studio. Transient failures
are **retried with bounded backoff** (honoring `Retry-After`):

- retried: `408`, `429`, `529`, other `5xx`, connect errors, timeout errors;
- **not** retried: `401` and `403` (fail fast), and validation/semantic errors.

---

## How it works

```text
brief
  ↓
Claude structured output
  ↓
schema validation
  ↓
retry with feedback if needed
  ↓
schema.json
  ↓
rustio-admin import
  ↓
rustio-admin plan
  ↓
human review
  ↓
rustio-admin commit
```

rustio-draft owns the top half (draft → protect → write). RustIO Admin owns the
bottom half (import → plan → commit), deterministically and with no AI.

---

## Other uses

rustio-draft is a **domain-to-schema thinking layer**, so it's useful beyond a
direct `import`.

**Available today** (built on `new`, `refine`, and the Studio):

- **Client discovery** — turn a business idea into a first data model during an
  early client meeting.
- **Schema review** — feed an existing schema to `refine` with an improvement
  instruction to propose better modeling.
- **Vertical templates** — generate starter schemas for domains like
  interpretation booking, waste logistics, clinics, POS, print shops, beauty
  salons, or field service.

**Future ideas** (not implemented yet):

- **Proposal generation** — turn a schema into a client-facing project summary.
- **UI blueprint** — derive admin navigation, pages, forms, filters, and
  dashboards from the schema.
- **Demo data** — generate realistic demo records from the schema.

---

## Limits

- Field types are intentionally limited to `text`, `integer`, `boolean`,
  `timestamp`.
- Relations are represented as plain integer `*_id` fields (e.g. `client_id`).
- Enums are represented as `text` for now.
- Money should be stored as `integer` minor units (e.g. cents) for now.
- It is not a migration engine.
- It does not guarantee the business model is correct — a human still reviews.

## Status

Implemented:

- **F1** engine
- **F2** `--apply` chain
- **F3** `refine`
- **F4** localhost Studio
- **F5** CI drift guard on `FIELD_TYPES`
- safety hardening (validation, diff, destructive-change protection)
- transport retry and timeouts

Current field types:

- `text`
- `integer`
- `boolean`
- `timestamp`

See the scope doc for the full design and roadmap.
