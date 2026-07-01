//! The local web wizard (`rustio-draft serve`) — F4.
//!
//! A localhost-only studio: enter a brief, get the proposed schema as editable
//! model/field cards, refine it in place, and download or save `schema.json`.
//! It's the same engine as the CLI behind a small HTTP API. **The Anthropic API
//! key stays server-side** — the browser only ever sees schema JSON.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::schema::{self, SchemaDoc};
use crate::{diff, DraftClient};

const STUDIO_HTML: &str = include_str!("../assets/studio.html");

/// Shared server state: the key + per-request defaults + the save target.
struct AppState {
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    out_path: PathBuf,
}

impl AppState {
    /// A client for one request, honoring per-request model/token overrides.
    fn client(&self, model: Option<String>, max_tokens: Option<u32>) -> DraftClient {
        DraftClient::new(
            self.api_key.clone(),
            model.unwrap_or_else(|| self.default_model.clone()),
            max_tokens.unwrap_or(self.default_max_tokens),
        )
    }
}

type ApiError = (StatusCode, Json<ErrResp>);

#[derive(Serialize)]
struct ErrResp {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    problems: Option<Vec<String>>,
}

/// Map an engine error (LLM/network) to a 502 with a JSON body.
fn upstream(e: anyhow::Error) -> ApiError {
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrResp {
            error: format!("{e:#}"),
            problems: None,
        }),
    )
}

#[derive(Deserialize)]
struct GenerateReq {
    brief: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct RefineReq {
    schema: SchemaDoc,
    instruction: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct SaveReq {
    schema: SchemaDoc,
    /// Opt-in to write destructive changes (removed models/fields, changed
    /// types, relaxed unique). The UI sends this after a "Save anyway" confirm.
    #[serde(default)]
    allow_destructive: bool,
}

#[derive(Serialize)]
struct SchemaResp {
    schema: SchemaDoc,
}

#[derive(Serialize, Debug)]
struct SaveResp {
    ok: bool,
    path: String,
}

async fn index() -> Html<&'static str> {
    Html(STUDIO_HTML)
}

/// The builder's field-type vocabulary, so the UI's `type` dropdowns match the
/// contract exactly (single source: [`schema::FIELD_TYPES`]).
async fn field_types() -> Json<&'static [&'static str]> {
    Json(schema::FIELD_TYPES)
}

async fn generate(
    State(s): State<Arc<AppState>>,
    Json(req): Json<GenerateReq>,
) -> Result<Json<SchemaResp>, ApiError> {
    let doc = s
        .client(req.model, req.max_tokens)
        .generate(&req.brief)
        .await
        .map_err(upstream)?;
    Ok(Json(SchemaResp { schema: doc }))
}

async fn refine(
    State(s): State<Arc<AppState>>,
    Json(req): Json<RefineReq>,
) -> Result<Json<SchemaResp>, ApiError> {
    let doc = s
        .client(req.model, req.max_tokens)
        // Studio refine is a preview; the destructive gate lives in the save
        // handler, so keep the model-preservation guard on here.
        .refine(&req.schema, &req.instruction, false)
        .await
        .map_err(upstream)?;
    Ok(Json(SchemaResp { schema: doc }))
}

/// Read and parse the schema currently at `path`, if it exists and is valid
/// JSON. Returns `None` (rather than erroring) when there's no usable baseline.
fn read_schema(path: &Path) -> Option<SchemaDoc> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

async fn save(
    State(s): State<Arc<AppState>>,
    Json(req): Json<SaveReq>,
) -> Result<Json<SaveResp>, ApiError> {
    // Validate exactly as `import` will before writing anything.
    if let Err(problems) = schema::validate(&req.schema) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrResp {
                error: "the schema has problems".into(),
                problems: Some(problems),
            }),
        ));
    }

    // Destructive-change guard, mirroring the CLI. Baseline is the schema
    // currently on disk (what this save would overwrite); a missing or
    // unparseable file means there's nothing to lose, so we skip the check.
    // On a destructive diff we refuse with 409 and the list of destructive
    // changes; the UI offers "Save anyway", which resends allow_destructive=true.
    if !req.allow_destructive {
        if let Some(current) = read_schema(&s.out_path) {
            let changes = diff::between(&current, &req.schema);
            if changes.is_destructive() {
                return Err((
                    StatusCode::CONFLICT,
                    Json(ErrResp {
                        error: "saving these changes is destructive — confirm to save anyway"
                            .into(),
                        problems: Some(changes.destructive_changes()),
                    }),
                ));
            }
        }
    }

    let pretty = serde_json::to_string_pretty(&req.schema).map_err(|e| upstream(e.into()))?;
    std::fs::write(&s.out_path, format!("{pretty}\n")).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrResp {
                error: format!("could not write {}: {e}", s.out_path.display()),
                problems: None,
            }),
        )
    })?;
    Ok(Json(SaveResp {
        ok: true,
        path: s.out_path.display().to_string(),
    }))
}

/// Build the studio router. Split out so it can be exercised without binding a
/// socket.
fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/field-types", get(field_types))
        .route("/api/generate", post(generate))
        .route("/api/refine", post(refine))
        .route("/api/save", post(save))
        .with_state(state)
}

/// Run the studio on `127.0.0.1:<port>` until interrupted. Localhost-only by
/// design — this is a dev tool and the API key lives in this process.
pub async fn run(
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    out_path: PathBuf,
    port: u16,
) -> Result<()> {
    let state = Arc::new(AppState {
        api_key,
        default_model,
        default_max_tokens,
        out_path,
    });
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;
    eprintln!("rustio-draft studio → http://{addr}  (Ctrl-C to stop)");
    axum::serve(listener, app(state))
        .await
        .context("studio server error")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_for(out_path: PathBuf) -> Arc<AppState> {
        Arc::new(AppState {
            api_key: "test-key".into(),
            default_model: "test-model".into(),
            default_max_tokens: 8000,
            out_path,
        })
    }

    fn parse(json: &str) -> SchemaDoc {
        serde_json::from_str(json).unwrap()
    }

    #[tokio::test]
    async fn destructive_save_returns_409_then_succeeds_with_opt_in() {
        // Baseline on disk: Client with two fields.
        let path = std::env::temp_dir().join("rustio_draft_save_409_test.json");
        std::fs::write(
            &path,
            r#"{"models":[{"name":"Client","fields":[{"name":"full_name","type":"text"},{"name":"phone","type":"text"}]}]}"#,
        )
        .unwrap();
        let state = state_for(path.clone());

        // Proposed save drops Client.phone → destructive.
        let dropped = parse(
            r#"{"models":[{"name":"Client","fields":[{"name":"full_name","type":"text"}]}]}"#,
        );

        // Without opt-in: refused with 409 and the destructive change listed.
        let refused = save(
            State(state.clone()),
            Json(SaveReq {
                schema: dropped.clone(),
                allow_destructive: false,
            }),
        )
        .await;
        let (code, body) = refused.unwrap_err();
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(
            body.0
                .problems
                .as_ref()
                .unwrap()
                .iter()
                .any(|p| p.contains("Client.phone")),
            "{:?}",
            body.0.problems
        );
        // The file must be untouched by a refused save.
        assert!(read_schema(&path).unwrap().models[0].fields.len() == 2);

        // With opt-in: it writes.
        let ok = save(
            State(state),
            Json(SaveReq {
                schema: dropped,
                allow_destructive: true,
            }),
        )
        .await;
        assert!(ok.is_ok());
        assert_eq!(read_schema(&path).unwrap().models[0].fields.len(), 1);

        std::fs::remove_file(&path).ok();
    }
}
