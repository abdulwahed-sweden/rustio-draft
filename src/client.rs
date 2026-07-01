//! The one place that talks to an LLM. Calls the Claude Messages API over raw
//! HTTP (there is no official Anthropic Rust SDK), constrains the response to
//! the import contract via structured outputs, and parses it into a [`SchemaDoc`].

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::schema::{self, SchemaDoc};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const API_VERSION: &str = "2023-06-01";

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
- Prefer a small, sensible set of fields per model over an exhaustive one.

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
        Self {
            api_key,
            model,
            max_tokens,
            http: reqwest::Client::new(),
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
    /// updated document. Like [`generate`](Self::generate), the result is already
    /// validated — and, additionally, guarded against silently dropping models
    /// that were in the input (see [`dropped_models`]).
    pub async fn refine(&self, current: &SchemaDoc, instruction: &str) -> Result<SchemaDoc> {
        let current_json = serde_json::to_string_pretty(current)
            .context("could not serialize the current schema")?;
        let user = format!(
            "Here is the current schema:\n\n```json\n{current_json}\n```\n\n\
             Apply this change: {instruction}\n\n\
             Return the COMPLETE updated schema. Every model and field already \
             present MUST still be there, plus the change. Do NOT drop any model \
             or field, and never return an empty `models` list."
        );
        self.complete_valid(user, |doc| dropped_models(current, doc))
            .await
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
            // Collect structural problems and caller-specific ones together, so
            // one retry can fix everything at once.
            let mut problems = schema::validate(&doc).err().unwrap_or_default();
            if let Err(extra) = extra_check(&doc) {
                problems.extend(extra);
            }
            if problems.is_empty() {
                return Ok(doc);
            }
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
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await
            .context("could not reach the Claude API")?;

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

    /// Verify the API key by listing available models (`GET /v1/models`).
    /// Returns the available model IDs on success. Cheap: no tokens are
    /// generated, so this is safe to run as a health check. Maps common auth
    /// failures to friendly messages.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(MODELS_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .send()
            .await
            .context("could not reach the Claude API")?;

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
