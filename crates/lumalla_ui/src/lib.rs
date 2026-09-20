//! Shared form-spec and NDJSON protocol types for `lumalla-ui` and `lumalla-config`.

#![warn(missing_docs)]

mod protocol;

pub use protocol::{
    ActionSpec, ChoiceOption, FieldSpec, FieldType, FieldUpdate, FieldValue, FormResult, FormSpec,
    UiEvent, UiReply, Values,
};
