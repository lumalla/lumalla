//! Shared form-spec and NDJSON protocol types for `lumalla-ui` and `lumalla-config`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Declarative form description (no Lua callbacks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormSpec {
    /// Window / form title.
    #[serde(default)]
    pub title: String,
    /// Ordered fields.
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
    /// Action buttons.
    #[serde(default)]
    pub actions: Vec<ActionSpec>,
}

/// One form field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldSpec {
    /// Stable id used in `values` maps.
    pub id: String,
    /// Field widget kind.
    #[serde(rename = "type")]
    pub field_type: FieldType,
    /// Visible label.
    #[serde(default)]
    pub label: String,
    /// Placeholder for text fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Initial value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<FieldValue>,
    /// Mask text input.
    #[serde(default)]
    pub password: bool,
    /// Request initial keyboard focus.
    #[serde(default)]
    pub focus: bool,
    /// Options for [`FieldType::Choice`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ChoiceOption>,
    /// Transient validation error shown under the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Supported field widgets (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// Free-form string.
    Text,
    /// Single-select from [`FieldSpec::options`].
    Choice,
    /// Boolean checkbox.
    Toggle,
    /// Static text (not included in values).
    Label,
}

/// Choice entry: either a bare string id or `{ id, label }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChoiceOption {
    /// Id used as both value and label.
    Id(String),
    /// Explicit id + label.
    Labeled {
        /// Value stored in results.
        id: String,
        /// Display label.
        #[serde(default)]
        label: String,
    },
}

impl ChoiceOption {
    /// Value id for this option.
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) => id,
            Self::Labeled { id, .. } => id,
        }
    }

    /// Display label for this option.
    pub fn label(&self) -> &str {
        match self {
            Self::Id(id) => id,
            Self::Labeled { id, label } => {
                if label.is_empty() {
                    id
                } else {
                    label
                }
            }
        }
    }
}

/// Action button.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    /// Stable action id passed to callbacks.
    pub id: String,
    /// Button label.
    #[serde(default)]
    pub label: String,
    /// Emphasize as the default action.
    #[serde(default)]
    pub primary: bool,
    /// Collect values and finish the form when pressed.
    #[serde(default)]
    pub submit: bool,
}

/// A field value in results / live updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    /// Text / choice.
    String(String),
    /// Toggle.
    Bool(bool),
}

impl FieldValue {
    /// Lua-friendly display (for logging); prefer typed accessors in Rust.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            Self::Bool(_) => None,
        }
    }

    /// Boolean if this is a toggle value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::String(_) => None,
        }
    }
}

/// Map of field id → value (ordered for stable JSON).
pub type Values = BTreeMap<String, FieldValue>;

/// Final result written on stdout in simple mode (exit 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormResult {
    /// Action id that finished the form (`submit` button).
    pub action: String,
    /// Collected field values (labels omitted).
    pub values: Values,
}

/// Helper → config (interactive stdout).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEvent {
    /// A field value changed.
    Change {
        /// Field id.
        id: String,
        /// New value for that field.
        value: FieldValue,
        /// Full values snapshot.
        values: Values,
    },
    /// Request validation before submit.
    Validate {
        /// Full values snapshot.
        values: Values,
    },
    /// Non-submit action pressed.
    Action {
        /// Action id.
        action: String,
        /// Full values snapshot.
        values: Values,
    },
    /// Submit action; config should reply before the helper exits.
    Submit {
        /// Action id.
        action: String,
        /// Full values snapshot.
        values: Values,
    },
    /// User cancelled (Escape / close / cancel button).
    Cancel,
}

/// Config → helper (interactive stdin).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiReply {
    /// Proceed / accept.
    Ok,
    /// Show a form-level error and keep the dialog open.
    Error {
        /// Message to show.
        message: String,
    },
    /// Patch field metadata (errors, defaults, options).
    Update {
        /// Field patches by id.
        #[serde(default)]
        fields: Vec<FieldUpdate>,
    },
    /// Close the helper window.
    Close,
}

/// Partial field update from config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldUpdate {
    /// Field id to patch.
    pub id: String,
    /// Replace error text (`null` clears).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Replace current value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<FieldValue>,
    /// Replace label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Replace placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Replace choice options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<ChoiceOption>>,
}

impl FormSpec {
    /// Collect current values from field defaults (labels skipped).
    pub fn initial_values(&self) -> Values {
        let mut values = Values::new();
        for field in &self.fields {
            if field.field_type == FieldType::Label {
                continue;
            }
            let value = field.default.clone().unwrap_or_else(|| match field.field_type {
                FieldType::Toggle => FieldValue::Bool(false),
                FieldType::Choice => FieldValue::String(
                    field
                        .options
                        .first()
                        .map(|o| o.id().to_owned())
                        .unwrap_or_default(),
                ),
                FieldType::Text | FieldType::Label => FieldValue::String(String::new()),
            });
            values.insert(field.id.clone(), value);
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_spec_roundtrip() {
        let spec = FormSpec {
            title: String::from("New view"),
            fields: vec![FieldSpec {
                id: String::from("name"),
                field_type: FieldType::Text,
                label: String::from("Name"),
                placeholder: Some(String::from("pip")),
                default: None,
                password: false,
                focus: true,
                options: vec![],
                error: None,
            }],
            actions: vec![ActionSpec {
                id: String::from("ok"),
                label: String::from("Create"),
                primary: true,
                submit: true,
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: FormSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, "New view");
        assert_eq!(parsed.fields[0].id, "name");
        assert!(parsed.actions[0].submit);
    }

    #[test]
    fn ui_event_tagged() {
        let event = UiEvent::Submit {
            action: String::from("ok"),
            values: Values::from([(
                String::from("name"),
                FieldValue::String(String::from("pip")),
            )]),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"submit""#));
        let parsed: UiEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, UiEvent::Submit { .. }));
    }
}
