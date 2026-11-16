//! Editor save identities and explicit host outcomes. None of these fields grants file access.

/// Character limits for opaque request/document/revision identities.
pub const MAX_SAVE_ID_CHARS: usize = 128;
/// A bounded, plain diagnostic; hosts must exclude credentials and private infrastructure details.
pub const MAX_SAVE_ERROR_CHARS: usize = 1024;

/// Identity captured by the editor at serialization, before asynchronous host work starts.
/// `revision` belongs to the live editor, not the account server's saved revision. The host
/// binds this context to its actual instance/session/account resource and server revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveContext<'a> {
    pub id: &'a str,
    pub document_id: &'a str,
    pub revision: &'a str,
    pub operation: &'a str,
    /// Whether successful storage makes these bytes the document's source. Exports are false.
    pub document_save: bool,
}

pub(crate) fn valid_save_id(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= MAX_SAVE_ID_CHARS && !value.chars().any(char::is_control)
}

impl SaveContext<'_> {
    /// Reject malformed metadata before copying or dispatching the file bytes.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_save_id(self.id) {
            return Err("Invalid save request identity");
        }
        if !valid_save_id(self.document_id) {
            return Err("Invalid editor document identity");
        }
        if !valid_save_id(self.revision) {
            return Err("Invalid editor revision");
        }
        if self.operation.is_empty() || self.operation.len() > 64 || !self.operation.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err("Invalid save operation");
        }
        Ok(())
    }
}

/// A host could not confirm a save. Unknown is deliberately distinct: a mutation may have
/// committed, so an editor must retain its request identity and must not blindly repeat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveFailureKind {
    Failed,
    Cancelled,
    Conflict,
    Unknown,
}

impl SaveFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Conflict => "conflict",
            Self::Unknown => "unknown",
        }
    }

    /// No default success/failure for an omitted or unrecognized outcome.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "conflict" => Some(Self::Conflict),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> SaveContext<'static> {
        SaveContext { id: "save-1", document_id: "doc-7", revision: "42", operation: "file.saveAs", document_save: true }
    }

    #[test]
    fn context_is_bounded_and_does_not_infer_intent_from_a_filename() {
        assert!(context().validate().is_ok());
        assert!(SaveContext { operation: "file.exportPdf", document_save: false, ..context() }.validate().is_ok());
        for bad in ["", "line\nbreak", "nul\0value"] {
            assert!(SaveContext { id: bad, ..context() }.validate().is_err());
            assert!(SaveContext { document_id: bad, ..context() }.validate().is_err());
            assert!(SaveContext { revision: bad, ..context() }.validate().is_err());
        }
        let too_long = "x".repeat(MAX_SAVE_ID_CHARS + 1);
        assert!(SaveContext { id: &too_long, ..context() }.validate().is_err());
        assert!(SaveContext { document_id: &too_long, ..context() }.validate().is_err());
        assert!(SaveContext { revision: &too_long, ..context() }.validate().is_err());
        for bad in ["", "file save", "file.save\n", "file/save"] {
            assert!(SaveContext { operation: bad, ..context() }.validate().is_err());
        }
    }

    #[test]
    fn host_failure_outcomes_are_explicit_and_round_trip() {
        for kind in [SaveFailureKind::Failed, SaveFailureKind::Cancelled, SaveFailureKind::Conflict, SaveFailureKind::Unknown] {
            assert_eq!(SaveFailureKind::parse(kind.as_str()), Some(kind));
        }
        for invalid in ["", "saved", "timeout", "FAILED", "unknown "] {
            assert_eq!(SaveFailureKind::parse(invalid), None);
        }
    }
}
