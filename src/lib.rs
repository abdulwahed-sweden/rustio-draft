//! rustio-draft — setup-time genesis for rustio-admin.
//!
//! Turns a natural-language brief into a `schema.json` (via the Claude API),
//! which `rustio-admin import` then applies deterministically. This crate is the
//! *only* place in the ecosystem that calls an LLM, and it lives in its own repo
//! (separate from the framework) so the runtime never gains a network/LLM dep.
//!
//! Full design and boundary:
//! <https://github.com/abdulwahed-sweden/rustio-admin/blob/main/docs/RUSTIO_DRAFT_SCOPE.md>.

pub mod client;
pub mod diff;
pub mod schema;
pub mod server;

pub use client::{DraftClient, ModelPreservationError, DEFAULT_MODEL};
pub use diff::SchemaDiff;
pub use schema::{validate, SchemaDoc, SchemaField, SchemaModel, FIELD_TYPES};

/// Resolve the `rustio-admin` binary to shell out to for `--apply`: an explicit
/// `--rustio-admin` flag wins, then `$RUSTIO_ADMIN_BIN`, else the bare name
/// `rustio-admin` (resolved on `PATH`).
pub fn resolve_rustio_admin_bin(explicit: Option<&str>) -> String {
    if let Some(b) = explicit {
        return b.to_string();
    }
    match std::env::var("RUSTIO_ADMIN_BIN") {
        Ok(b) if !b.is_empty() => b,
        _ => "rustio-admin".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_rustio_admin_bin;

    #[test]
    fn explicit_flag_wins() {
        assert_eq!(
            resolve_rustio_admin_bin(Some("/opt/bin/rustio-admin")),
            "/opt/bin/rustio-admin"
        );
    }
}
