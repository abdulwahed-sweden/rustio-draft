//! The one place that talks to an LLM. Calls the Claude Messages API over raw
//! HTTP (there is no official Anthropic Rust SDK), constrains the response to
//! the import contract via structured outputs, and parses it into a [`SchemaDoc`].

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::schema::{self, SchemaDoc};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const API_VERSION: &str = "2023-06-01";

/// Fail fast when the network is unreachable rather than hanging on connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total per-request cap. Responses aren't streamed, so the whole body (thinking
/// + tokens) must arrive within this; generous because schemas are small.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Total transport attempts (1 initial + 2 retries) for transient failures.
const MAX_HTTP_ATTEMPTS: usize = 3;
/// Deterministic backoff before the 1st and 2nd retry: 500ms, then 1s, then 2s
/// (the last entry is reused/doubled if ever needed). Used when the server
/// doesn't tell us otherwise via `Retry-After`.
const BACKOFF: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// Upper bound on a server-provided `Retry-After`, so a hostile or misconfigured
/// value (e.g. `Retry-After: 86400`) can't make the CLI sleep for a long time.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Default model. Overridable on the CLI; this is the reference default.
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// How many times to ask the model for a valid schema before giving up. The
/// first attempt is plain; later attempts feed the validation problems back.
const MAX_ATTEMPTS: usize = 2;

/// System prompt: teach the model the contract and the field-type vocabulary.
const SYSTEM_PROMPT: &str = "\
You are a database schema designer for rustio-admin, a Postgres admin framework. \
Given a short description of an application (or an existing schema plus an edit \
instruction), produce a clean, normalised set of models.

Rules you MUST follow:
- Use ONLY these field types: text, integer, boolean, timestamp.
- Model names are CamelCase singular nouns (Client, Appointment), no spaces.
- Field names are snake_case (full_name, starts_at).
- NEVER add an `id` field or a `created_at` field — the generator emits those implicitly.
- Map money/prices/amounts to `integer` (store minor units, e.g. cents).
- Map dates and date-times to `timestamp`. Map counts/quantities to `integer`.
- Relations are out of scope for now: instead of a foreign-key field, add a plain \
`integer` field named `<thing>_id` (e.g. client_id) and note nothing else.
- Give EVERY model a complete, useful set of fields — normally 4 to 8 — covering \
its real attributes plus any `<thing>_id` relations. NEVER emit a model with only \
one field; if a model seems to need just one, add the other attributes that entity \
naturally has.

Return only the schema in the required JSON shape — no prose.";

/// A configured client for one generation run.
pub struct DraftClient {
    api_key: String,
    model: String,
    max_tokens: u32,
    http: reqwest::Client,
}

impl DraftClient {
    /// Build a client. `api_key` is the Anthropic API key; `model` and
    /// `max_tokens` come from the CLI (with [`DEFAULT_MODEL`] as the default).
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        // Build with connect/request timeouts so a slow or hung upstream can't
        // stall the CLI or a studio request forever. `build()` only fails on TLS
        // backend init — fall back to the default client rather than panic.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            api_key,
            model,
            max_tokens,
            http,
        }
    }

    /// Turn a natural-language brief into a [`SchemaDoc`]. The returned document
    /// has already passed [`schema::validate`]; callers re-check it before
    /// writing as a final gate.
    pub async fn generate(&self, brief: &str) -> Result<SchemaDoc> {
        // A fresh design has nothing to preserve, so no extra check.
        self.complete_valid(brief.to_string(), |_| Ok(())).await
    }

    /// Apply an edit instruction to an existing schema and return the COMPLETE
    /// updated document. The result is already validated.
    ///
    /// When `allow_destructive` is `false`, a model-preservation guard
    /// ([`dropped_models`]) makes the model retry if it drops a model — this
    /// recovers additive edits from the model's occasional over-eager
    /// restructuring. When `allow_destructive` is `true`, that guard is disabled
    /// so an intentional model removal can go through; the caller's deterministic
    /// diff gate is then the sole authority on what is destructive.
    pub async fn refine(
        &self,
        current: &SchemaDoc,
        instruction: &str,
        allow_destructive: bool,
    ) -> Result<SchemaDoc> {
        let current_json = serde_json::to_string_pretty(current)
            .context("could not serialize the current schema")?;
        let user = format!(
            "Here is the current schema:\n\n```json\n{current_json}\n```\n\n\
             Apply this change: {instruction}\n\n\
             Return the COMPLETE updated schema. Every model and field already \
             present MUST still be there, plus the change. Do NOT drop any model \
             or field, and never return an empty `models` list."
        );
        if allow_destructive {
            self.complete_valid(user, |_| Ok(())).await
        } else {
            self.complete_valid(user, |doc| dropped_models(current, doc))
                .await
        }
    }

    /// Ask the model for a schema and keep it only if it passes
    /// [`schema::validate`] **and** `extra_check`; on failure, re-ask up to
    /// [`MAX_ATTEMPTS`] times, feeding the concrete problems back so the model can
    /// correct itself. Shared by [`generate`](Self::generate) and
    /// [`refine`](Self::refine) (and thus the CLI and the studio), so every entry
    /// point is equally robust against an invalid or lossy response. `extra_check`
    /// is a caller-supplied validator for guarantees `schema::validate` can't see
    /// — e.g. refine's "don't drop existing models" rule.
    async fn complete_valid(
        &self,
        base_user: String,
        extra_check: impl Fn(&SchemaDoc) -> Result<(), Vec<String>>,
    ) -> Result<SchemaDoc> {
        let mut feedback: Option<Vec<String>> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let doc = self
                .complete(base_user.clone(), feedback.as_deref())
                .await?;
            // Keep structural (schema) problems and the caller's guard problems
            // separate so the final error can be classified, but feed them back
            // together so one retry can fix everything at once.
            let structural = schema::validate(&doc).err().unwrap_or_default();
            let guard = extra_check(&doc).err().unwrap_or_default();
            if structural.is_empty() && guard.is_empty() {
                return Ok(doc);
            }
            let mut problems = structural.clone();
            problems.extend(guard.iter().cloned());
            if attempt < MAX_ATTEMPTS {
                eprintln!(
                    "The model returned an invalid schema (attempt {attempt}/{MAX_ATTEMPTS}) — retrying…"
                );
                for p in &problems {
                    eprintln!("  - {p}");
                }
                // Feed the problems back so the next attempt is a targeted
                // correction rather than a blind re-roll.
                feedback = Some(problems);
            } else if structural.is_empty() && !guard.is_empty() {
                // The schema itself is fine — the only reason we reject it is the
                // caller's guard (refine's model-preservation rule). Surface a
                // typed error so the CLI can explain it clearly.
                return Err(ModelPreservationError { problems: guard }.into());
            } else {
                return Err(invalid_schema_error(&problems));
            }
        }
        unreachable!("loop returns or errors on the final attempt")
    }

    /// One Messages API round-trip with structured output, returning a parsed
    /// [`SchemaDoc`]. When `problems` is set, the previous attempt's validation
    /// failures are appended so the model can correct them. Shared by
    /// [`generate`](Self::generate) and [`refine`](Self::refine).
    async fn complete(&self, base_user: String, problems: Option<&[String]>) -> Result<SchemaDoc> {
        let user_content = match problems {
            None => base_user,
            Some(problems) => {
                let listed = problems
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "{base_user}\n\nYour previous response was invalid:\n{listed}\n\n\
                     Return a corrected schema that fixes every problem above."
                )
            }
        };
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "thinking": { "type": "adaptive" },
            "system": SYSTEM_PROMPT,
            "output_config": {
                "format": { "type": "json_schema", "schema": schema::import_json_schema() }
            },
            "messages": [ { "role": "user", "content": user_content } ]
        });

        let resp = self
            .send_retrying(|| {
                self.http
                    .post(API_URL)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", API_VERSION)
                    .json(&body)
            })
            .await?;

        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .context("Claude API returned a non-JSON response")?;

        if !status.is_success() {
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            bail!("Claude API error ({status}): {msg}");
        }

        parse_schema_response(&v)
    }

    /// Send a request with bounded retry on transient failures (`408`, `429`,
    /// any `5xx` including `529`, and connect/timeout transport errors). `build`
    /// is called once per attempt so the request can be re-issued. Auth and
    /// validation failures (`4xx` other than 408/429) are returned immediately.
    async fn send_retrying(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut attempt = 1;
        loop {
            match build().send().await {
                Ok(resp) if is_retryable(resp.status()) && attempt < MAX_HTTP_ATTEMPTS => {
                    let delay = backoff_delay(attempt, retry_after_from(&resp));
                    eprintln!(
                        "Claude API returned {} — retrying in {:.1}s (attempt {attempt}/{MAX_HTTP_ATTEMPTS})…",
                        resp.status(),
                        delay.as_secs_f64()
                    );
                    tokio::time::sleep(delay).await;
                }
                Ok(resp) => return Ok(resp),
                Err(e) if is_retryable_error(&e) && attempt < MAX_HTTP_ATTEMPTS => {
                    let delay = backoff_delay(attempt, None);
                    eprintln!(
                        "Claude API request failed ({e}) — retrying in {:.1}s (attempt {attempt}/{MAX_HTTP_ATTEMPTS})…",
                        delay.as_secs_f64()
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(anyhow!(e).context("could not reach the Claude API")),
            }
            attempt += 1;
        }
    }

    /// Verify the API key by listing available models (`GET /v1/models`).
    /// Returns the available model IDs on success. Cheap: no tokens are
    /// generated, so this is safe to run as a health check. Maps common auth
    /// failures to friendly messages.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self
            .send_retrying(|| {
                self.http
                    .get(MODELS_URL)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", API_VERSION)
            })
            .await?;

        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .context("Claude API returned a non-JSON response")?;

        if !status.is_success() {
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            match status.as_u16() {
                401 => bail!("API key is invalid or revoked (401): {msg}"),
                403 => bail!("API key lacks permission (403): {msg}"),
                _ => bail!("Claude API error ({status}): {msg}"),
            }
        }

        Ok(parse_model_ids(&v))
    }
}

/// Extract model IDs from a `GET /v1/models` response. Pulled out of the network
/// path so it can be unit-tested.
fn parse_model_ids(v: &Value) -> Vec<String> {
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Error returned by [`refine`](DraftClient::refine) when the *only* reason the
/// result was rejected is the model-preservation guard — i.e. the model kept
/// dropping existing models during a non-destructive refine. Distinguished from a
/// plain invalid schema so the CLI can tell the user to use `--allow-destructive`
/// if the removal was intended. (Only produced when `allow_destructive` is off,
/// since that is the only time the guard runs.)
#[derive(Debug)]
pub struct ModelPreservationError {
    /// The per-model "… is missing" problems from the last attempt.
    pub problems: Vec<String>,
}

impl std::fmt::Display for ModelPreservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model kept dropping existing models across {MAX_ATTEMPTS} attempts"
        )
    }
}

impl std::error::Error for ModelPreservationError {}

/// Guard for `refine`: report any model that was in `before` but is missing from
/// `after`. Refine edits in place by default, so a lossy result would overwrite
/// the user's schema — [`complete_valid`](DraftClient::complete_valid) retries on
/// these, then refuses to write. Field *removals* are intentionally allowed (a
/// refine like "remove the phone field" is legitimate); dropping a whole model
/// during an edit is the model misbehaving.
fn dropped_models(before: &SchemaDoc, after: &SchemaDoc) -> Result<(), Vec<String>> {
    let kept: std::collections::HashSet<&str> =
        after.models.iter().map(|m| m.name.as_str()).collect();
    let problems: Vec<String> = before
        .models
        .iter()
        .map(|m| m.name.as_str())
        .filter(|name| !kept.contains(name))
        .map(|name| {
            format!(
                "model '{name}' from the input is missing — return the COMPLETE \
                 schema with every existing model preserved, plus the requested change"
            )
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Build the "invalid schema after N attempts" error, listing every problem.
fn invalid_schema_error(problems: &[String]) -> anyhow::Error {
    let mut msg = format!("the model returned an invalid schema after {MAX_ATTEMPTS} attempts:");
    for p in problems {
        msg.push_str(&format!("\n  - {p}"));
    }
    anyhow!(msg)
}

/// Whether an HTTP status is worth retrying: request-timeout (408), rate-limit
/// (429), or any server error (500–599, which includes 529 Overloaded). Auth and
/// other client errors (400/401/403/…) are never retried.
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Whether a transport error is worth retrying: a connection failure or a
/// timeout (as opposed to, say, a malformed-URL error).
fn is_retryable_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Backoff before re-issuing a request. `attempt` is the 1-based number of the
/// attempt that just failed. A server-provided `Retry-After` wins but is clamped
/// to [`MAX_RETRY_AFTER`]; otherwise use the deterministic [`BACKOFF`] schedule
/// (clamped to its last entry).
fn backoff_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    if let Some(d) = retry_after {
        return d.min(MAX_RETRY_AFTER);
    }
    let idx = (attempt - 1).min(BACKOFF.len() - 1);
    BACKOFF[idx]
}

/// Parse a `Retry-After` header given as an integer number of seconds. Ignores
/// the HTTP-date form (Anthropic sends seconds) and anything unparseable.
fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    value?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Extract and parse a response's `Retry-After` header, if any.
fn retry_after_from(resp: &reqwest::Response) -> Option<Duration> {
    let raw = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    parse_retry_after(raw.to_str().ok())
}

/// Extract and parse the schema from a successful Messages API response.
/// Pulled out of the network path so it can be unit-tested.
fn parse_schema_response(v: &Value) -> Result<SchemaDoc> {
    match v.get("stop_reason").and_then(|s| s.as_str()) {
        Some("refusal") => bail!(
            "the model declined this request (stop_reason: refusal). Try rephrasing the brief."
        ),
        Some("max_tokens") => bail!(
            "the response hit the token limit before the schema was complete; \
             re-run with a larger --max-tokens"
        ),
        _ => {}
    }

    let text = v
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("unexpected API response: no content array"))?
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("the response contained no text block to parse"))?;

    serde_json::from_str(text).context("the model's output was not a valid schema document")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> SchemaDoc {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn retryable_statuses_include_408_429_529_and_5xx() {
        for code in [408, 429, 500, 502, 503, 504, 529] {
            assert!(
                is_retryable(StatusCode::from_u16(code).unwrap()),
                "{code} should be retryable"
            );
        }
    }

    #[test]
    fn non_retryable_statuses_exclude_400_401_403_and_success() {
        for code in [200, 400, 401, 403, 404, 422] {
            assert!(
                !is_retryable(StatusCode::from_u16(code).unwrap()),
                "{code} should not be retryable"
            );
        }
    }

    #[test]
    fn retry_after_header_is_honored_over_backoff() {
        // A small, parseable Retry-After (seconds) wins over the schedule…
        assert_eq!(
            backoff_delay(1, parse_retry_after(Some("7"))),
            Duration::from_secs(7)
        );
        assert_eq!(
            parse_retry_after(Some("  10 ")),
            Some(Duration::from_secs(10))
        );
        // …and unparseable / missing values fall back to the schedule.
        assert_eq!(parse_retry_after(Some("soon")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn huge_retry_after_is_clamped() {
        // A hostile/misconfigured Retry-After can't make us sleep forever.
        assert_eq!(
            backoff_delay(1, parse_retry_after(Some("86400"))),
            MAX_RETRY_AFTER
        );
        // Exactly at the cap is unchanged; just under the cap is honored as-is.
        assert_eq!(backoff_delay(1, Some(MAX_RETRY_AFTER)), MAX_RETRY_AFTER);
        assert_eq!(
            backoff_delay(1, Some(Duration::from_secs(59))),
            Duration::from_secs(59)
        );
    }

    #[test]
    fn backoff_schedule_is_500ms_1s_2s_then_clamps() {
        assert_eq!(backoff_delay(1, None), Duration::from_millis(500));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, None), Duration::from_secs(2));
        // Beyond the schedule, stay at the last (2s) rather than panic.
        assert_eq!(backoff_delay(9, None), Duration::from_secs(2));
    }

    #[test]
    fn dropped_models_flags_missing_and_allows_field_removal() {
        let before = doc(r#"{ "models": [
                { "name": "Client", "fields": [ { "name": "full_name", "type": "text" }, { "name": "phone", "type": "text" } ] },
                { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] } ] }"#);

        // A model vanished → a problem naming it.
        let lossy = doc(
            r#"{ "models": [ { "name": "Client", "fields": [ { "name": "full_name", "type": "text" } ] } ] }"#,
        );
        let errs = dropped_models(&before, &lossy).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("Appointment"), "{errs:?}");

        // Same models, a field removed from Client → allowed (no problem).
        let field_removed = doc(r#"{ "models": [
                { "name": "Client", "fields": [ { "name": "full_name", "type": "text" } ] },
                { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] } ] }"#);
        assert!(dropped_models(&before, &field_removed).is_ok());
    }

    #[test]
    fn parses_structured_text_block() {
        // Mimics an adaptive-thinking response: an (empty) thinking block then text.
        let v = json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "thinking", "thinking": "" },
                { "type": "text", "text": "{ \"models\": [ { \"name\": \"Client\", \"fields\": [ { \"name\": \"full_name\", \"type\": \"text\" } ] } ] }" }
            ]
        });
        let doc = parse_schema_response(&v).unwrap();
        assert_eq!(doc.models.len(), 1);
        assert_eq!(doc.models[0].name, "Client");
    }

    #[test]
    fn refusal_is_a_clean_error() {
        let v = json!({ "stop_reason": "refusal", "content": [] });
        let err = parse_schema_response(&v).unwrap_err().to_string();
        assert!(err.contains("refusal"), "{err}");
    }

    #[test]
    fn truncation_is_a_clean_error() {
        let v = json!({ "stop_reason": "max_tokens", "content": [] });
        let err = parse_schema_response(&v).unwrap_err().to_string();
        assert!(err.contains("max-tokens"), "{err}");
    }

    #[test]
    fn parses_model_ids_in_order() {
        let v = json!({ "data": [
            { "id": "claude-opus-4-8", "type": "model" },
            { "id": "claude-sonnet-4-6", "type": "model" }
        ] });
        assert_eq!(
            parse_model_ids(&v),
            vec!["claude-opus-4-8", "claude-sonnet-4-6"]
        );
    }

    #[test]
    fn missing_data_yields_empty() {
        assert!(parse_model_ids(&json!({})).is_empty());
    }
}
