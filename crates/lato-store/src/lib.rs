mod file;
mod memory;
mod metadata;
mod writer;

pub use file::{FaultPoint, FileEventStore, FileFaultInjector};
pub use memory::MemoryEventStore;
pub use metadata::{
    SessionMetadata, SessionSummary, TitleSource, derive_automatic_title, normalize_manual_title,
};

pub const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_JOURNAL_RECORDS: usize = 100_000;

pub(crate) fn validate_capacity(
    envelopes: &[lato_core::JournalEnvelope],
    sequence: u64,
) -> Result<(), lato_core::JournalError> {
    if envelopes.len() > MAX_JOURNAL_RECORDS {
        return Err(lato_core::JournalError::Full {
            sequence,
            limit: MAX_JOURNAL_RECORDS as u64,
        });
    }
    let bytes = envelopes.iter().try_fold(0_u64, |total, envelope| {
        let line_bytes = serde_json::to_vec(envelope)
            .map_err(|error| lato_core::JournalError::Io {
                message: error.to_string(),
            })?
            .len() as u64
            + 1;
        Ok::<_, lato_core::JournalError>(total.saturating_add(line_bytes))
    })?;
    if bytes > MAX_JOURNAL_BYTES {
        return Err(lato_core::JournalError::Full {
            sequence,
            limit: MAX_JOURNAL_BYTES,
        });
    }
    Ok(())
}
