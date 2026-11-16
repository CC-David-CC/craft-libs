//! The identity returned after a host-provided file actually opens in the editor.
use crate::save::valid_save_id;

/// A completed binary open, captured before any later command can change focus.
/// `source_retained` is false for converted/repaired/unnamed copies. None of these
/// values authorizes account access or substitutes for an account server revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenResult<'a> {
    pub id: &'a str,
    pub document_id: &'a str,
    pub revision: &'a str,
    pub source_retained: bool,
}

impl OpenResult<'_> {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_save_id(self.id) {
            return Err("Invalid open request identity");
        }
        if !valid_save_id(self.document_id) {
            return Err("Invalid opened document identity");
        }
        if !valid_save_id(self.revision) {
            return Err("Invalid opened editor revision");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opened_identities_are_bounded_and_source_intent_is_explicit() {
        let result = OpenResult { id: "open-1", document_id: "doc-2", revision: "rev-3", source_retained: true };
        assert!(result.validate().is_ok());
        assert!(OpenResult { source_retained: false, ..result }.validate().is_ok());
        let oversized = "x".repeat(crate::MAX_SAVE_ID_CHARS + 1);
        for invalid in ["", "bad\nid", oversized.as_str()] {
            assert!(OpenResult { id: invalid, ..result }.validate().is_err());
            assert!(OpenResult { document_id: invalid, ..result }.validate().is_err());
            assert!(OpenResult { revision: invalid, ..result }.validate().is_err());
        }
    }
}
