//! A deterministic semantic diff between two [`SchemaDoc`]s.
//!
//! This is a *safety layer* for AI-generated `refine` output — **not** a
//! migration engine. It classifies the change from the current schema to the
//! model's proposed one so the CLI can (a) show the user exactly what changed and
//! (b) refuse destructive edits unless they explicitly opt in with
//! `--allow-destructive`. It complements the model-preservation guard in
//! [`crate::client`]: that guard catches dropped *models*; this layer also
//! catches dropped *fields*, changed field types, and relaxed `unique` flags —
//! things a structurally-valid schema can hide.

use std::collections::BTreeMap;

use crate::schema::{SchemaDoc, SchemaField, SchemaModel};

/// A single field's type change, `from` → `to`. `field` is `"Model.field"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeChange {
    pub field: String,
    pub from: String,
    pub to: String,
}

/// A single field's `unique`-flag change, `from` → `to`. `field` is
/// `"Model.field"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueChange {
    pub field: String,
    pub from: bool,
    pub to: bool,
}

/// The classified difference between two schemas. Every list is sorted, so the
/// diff — and its printed summary — is deterministic regardless of the model's
/// field ordering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaDiff {
    pub added_models: Vec<String>,
    pub removed_models: Vec<String>,
    /// `"Model.field"`, for models present in both schemas.
    pub added_fields: Vec<String>,
    /// `"Model.field"`, for models present in both schemas.
    pub removed_fields: Vec<String>,
    pub changed_types: Vec<TypeChange>,
    pub unique_changes: Vec<UniqueChange>,
}

/// Compute the semantic diff from `old` to `new`.
///
/// Models are matched by name; fields are matched by name within a model that
/// exists in both schemas. A whole model that appears/disappears is reported at
/// the model level only (its fields are implied), so `added_fields` /
/// `removed_fields` cover *edits to surviving models* — exactly the changes a
/// refine can smuggle in.
pub fn between(old: &SchemaDoc, new: &SchemaDoc) -> SchemaDiff {
    let old_models: BTreeMap<&str, &SchemaModel> =
        old.models.iter().map(|m| (m.name.as_str(), m)).collect();
    let new_models: BTreeMap<&str, &SchemaModel> =
        new.models.iter().map(|m| (m.name.as_str(), m)).collect();

    let mut d = SchemaDiff {
        added_models: new_models
            .keys()
            .filter(|n| !old_models.contains_key(*n))
            .map(|n| n.to_string())
            .collect(),
        removed_models: old_models
            .keys()
            .filter(|n| !new_models.contains_key(*n))
            .map(|n| n.to_string())
            .collect(),
        ..Default::default()
    };

    // Field-level diffs, only for models present in both schemas.
    for (name, old_m) in &old_models {
        let Some(new_m) = new_models.get(name) else {
            continue;
        };
        let old_fields: BTreeMap<&str, &SchemaField> =
            old_m.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        let new_fields: BTreeMap<&str, &SchemaField> =
            new_m.fields.iter().map(|f| (f.name.as_str(), f)).collect();

        for (fname, nf) in &new_fields {
            match old_fields.get(fname) {
                None => d.added_fields.push(format!("{name}.{fname}")),
                Some(of) => {
                    if of.ty != nf.ty {
                        d.changed_types.push(TypeChange {
                            field: format!("{name}.{fname}"),
                            from: of.ty.clone(),
                            to: nf.ty.clone(),
                        });
                    }
                    if of.unique != nf.unique {
                        d.unique_changes.push(UniqueChange {
                            field: format!("{name}.{fname}"),
                            from: of.unique,
                            to: nf.unique,
                        });
                    }
                }
            }
        }
        for fname in old_fields.keys() {
            if !new_fields.contains_key(fname) {
                d.removed_fields.push(format!("{name}.{fname}"));
            }
        }
    }

    // Sort everything for deterministic output. (Model lists come from BTreeMaps
    // and are already sorted; field lists are sorted per-model but concatenated
    // across models, so sort them too.)
    d.added_models.sort();
    d.removed_models.sort();
    d.added_fields.sort();
    d.removed_fields.sort();
    d.changed_types.sort_by(|a, b| a.field.cmp(&b.field));
    d.unique_changes.sort_by(|a, b| a.field.cmp(&b.field));
    d
}

impl SchemaDiff {
    /// Whether the two schemas are identical (nothing to report).
    pub fn is_empty(&self) -> bool {
        self.added_models.is_empty()
            && self.removed_models.is_empty()
            && self.added_fields.is_empty()
            && self.removed_fields.is_empty()
            && self.changed_types.is_empty()
            && self.unique_changes.is_empty()
    }

    /// Whether this diff loses information or invalidates the contract: a removed
    /// model or field, any field-type change, or a `unique` flag going from
    /// `true` to `false`. (Additive changes — new models/fields, or tightening a
    /// field to `unique` — are safe.)
    pub fn is_destructive(&self) -> bool {
        !self.removed_models.is_empty()
            || !self.removed_fields.is_empty()
            || !self.changed_types.is_empty()
            || self.unique_changes.iter().any(|c| c.from && !c.to)
    }

    /// The gate the CLI uses before writing a refined schema: block the write
    /// when the diff is destructive and the user has not opted in.
    pub fn blocks_write(&self, allow_destructive: bool) -> bool {
        self.is_destructive() && !allow_destructive
    }

    /// A human-readable, deterministic summary with one section per change kind.
    /// Empty sections print `- none` so the report shape is always the same.
    pub fn summary(&self) -> String {
        let types: Vec<String> = self
            .changed_types
            .iter()
            .map(|c| format!("{}: {} -> {}", c.field, c.from, c.to))
            .collect();
        let uniques: Vec<String> = self
            .unique_changes
            .iter()
            .map(|c| format!("{}: {} -> {}", c.field, c.from, c.to))
            .collect();
        let sections: [(&str, &[String]); 6] = [
            ("Added models", &self.added_models),
            ("Removed models", &self.removed_models),
            ("Added fields", &self.added_fields),
            ("Removed fields", &self.removed_fields),
            ("Changed field types", &types),
            ("Changed unique flags", &uniques),
        ];
        sections
            .iter()
            .map(|(title, items)| {
                let body = if items.is_empty() {
                    "- none".to_string()
                } else {
                    items
                        .iter()
                        .map(|i| format!("- {i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                format!("{title}:\n{body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> SchemaDoc {
        serde_json::from_str(json).unwrap()
    }

    /// A 3-model baseline reused across the destructive-change tests.
    fn base() -> SchemaDoc {
        doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": true },
                { "name": "phone", "type": "text" } ] },
            { "name": "Staff", "fields": [
                { "name": "name", "type": "text" } ] },
            { "name": "Appointment", "fields": [
                { "name": "starts_at", "type": "timestamp" } ] } ] }"#)
    }

    #[test]
    fn additive_refine_is_allowed() {
        // Add a whole model AND a field to an existing model — both additive.
        let after = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": true },
                { "name": "phone", "type": "text" },
                { "name": "cancelled", "type": "boolean" } ] },
            { "name": "Staff", "fields": [ { "name": "name", "type": "text" } ] },
            { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] },
            { "name": "Assignment", "fields": [ { "name": "role", "type": "text" } ] } ] }"#);
        let d = between(&base(), &after);
        assert!(!d.is_destructive());
        assert!(
            !d.blocks_write(false),
            "additive change must not be blocked"
        );
        assert_eq!(d.added_models, vec!["Assignment"]);
        assert_eq!(d.added_fields, vec!["Client.cancelled"]);
    }

    #[test]
    fn removed_model_is_refused_without_flag_but_allowed_with() {
        let after = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": true },
                { "name": "phone", "type": "text" } ] },
            { "name": "Staff", "fields": [ { "name": "name", "type": "text" } ] } ] }"#);
        let d = between(&base(), &after);
        assert_eq!(d.removed_models, vec!["Appointment"]);
        assert!(d.is_destructive());
        assert!(
            d.blocks_write(false),
            "removed model must be blocked by default"
        );
        assert!(!d.blocks_write(true), "--allow-destructive must permit it");
    }

    #[test]
    fn removed_field_is_refused_without_flag_but_allowed_with() {
        // Drop Client.phone — models all preserved, but a field vanished.
        let after = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": true } ] },
            { "name": "Staff", "fields": [ { "name": "name", "type": "text" } ] },
            { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] } ] }"#);
        let d = between(&base(), &after);
        assert_eq!(d.removed_fields, vec!["Client.phone"]);
        assert!(d.is_destructive());
        assert!(d.blocks_write(false));
        assert!(!d.blocks_write(true));
    }

    #[test]
    fn changed_field_type_is_refused_without_flag_but_allowed_with() {
        // Client.phone: text -> integer.
        let after = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": true },
                { "name": "phone", "type": "integer" } ] },
            { "name": "Staff", "fields": [ { "name": "name", "type": "text" } ] },
            { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] } ] }"#);
        let d = between(&base(), &after);
        assert_eq!(d.changed_types.len(), 1);
        assert_eq!(d.changed_types[0].field, "Client.phone");
        assert_eq!(
            (
                d.changed_types[0].from.as_str(),
                d.changed_types[0].to.as_str()
            ),
            ("text", "integer")
        );
        assert!(d.is_destructive());
        assert!(d.blocks_write(false));
        assert!(!d.blocks_write(true));
    }

    #[test]
    fn unique_true_to_false_is_refused_but_false_to_true_is_allowed() {
        // Relax Client.email from unique -> not unique: destructive.
        let relaxed = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text" },
                { "name": "phone", "type": "text" } ] },
            { "name": "Staff", "fields": [ { "name": "name", "type": "text" } ] },
            { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] } ] }"#);
        let d = between(&base(), &relaxed);
        assert_eq!(d.unique_changes.len(), 1);
        assert_eq!(
            (d.unique_changes[0].from, d.unique_changes[0].to),
            (true, false)
        );
        assert!(d.is_destructive());
        assert!(d.blocks_write(false));
        assert!(!d.blocks_write(true));

        // Tighten Client.phone to unique: reported, but NOT destructive.
        let tightened = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": true },
                { "name": "phone", "type": "text", "unique": true } ] },
            { "name": "Staff", "fields": [ { "name": "name", "type": "text" } ] },
            { "name": "Appointment", "fields": [ { "name": "starts_at", "type": "timestamp" } ] } ] }"#);
        let d2 = between(&base(), &tightened);
        assert_eq!(d2.unique_changes.len(), 1);
        assert_eq!(
            (d2.unique_changes[0].from, d2.unique_changes[0].to),
            (false, true)
        );
        assert!(!d2.is_destructive(), "false -> true is additive");
        assert!(!d2.blocks_write(false));
    }

    #[test]
    fn summary_reports_added_removed_and_changed_items() {
        // A diff exercising every section at once.
        let after = doc(r#"{ "models": [
            { "name": "Client", "fields": [
                { "name": "full_name", "type": "text" },
                { "name": "email", "type": "text", "unique": false },
                { "name": "age", "type": "integer" } ] },
            { "name": "Appointment", "fields": [
                { "name": "starts_at", "type": "integer" } ] },
            { "name": "Invoice", "fields": [ { "name": "amount", "type": "integer" } ] } ] }"#);
        let d = between(&base(), &after);
        let s = d.summary();
        // Added model Invoice; removed model Staff.
        assert!(s.contains("Added models:\n- Invoice"), "{s}");
        assert!(s.contains("Removed models:\n- Staff"), "{s}");
        // Client.age added; Client.phone removed.
        assert!(s.contains("- Client.age"), "{s}");
        assert!(s.contains("- Client.phone"), "{s}");
        // Appointment.starts_at: timestamp -> integer.
        assert!(
            s.contains("Appointment.starts_at: timestamp -> integer"),
            "{s}"
        );
        // Client.email: true -> false.
        assert!(s.contains("Client.email: true -> false"), "{s}");
    }

    #[test]
    fn identical_schemas_diff_empty_and_non_destructive() {
        let d = between(&base(), &base());
        assert!(d.is_empty());
        assert!(!d.is_destructive());
        assert!(d.summary().contains("Added models:\n- none"));
    }
}
