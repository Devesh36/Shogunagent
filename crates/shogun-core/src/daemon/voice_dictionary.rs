//! Personal voice-dictionary persistence.

use super::*;

impl Db {
    /// Snapshot built-ins plus persisted user terms. A storage fault preserves dictation with
    /// built-ins rather than turning a local DB hiccup into a transcription failure.
    pub fn voice_dictionary(&self) -> crate::voice_dictionary::VoiceDictionary {
        let entries = self
            .with_conn(
                "voice_terms.list_for_dictation",
                voice_terms::list_voice_terms,
            )
            .unwrap_or_default()
            .into_iter()
            .map(dictionary_entry)
            .collect();
        crate::voice_dictionary::VoiceDictionary::with_user_entries(entries)
    }

    /// Every persisted term, disabled included, for settings and API management.
    pub fn list_voice_terms(&self) -> Result<Vec<VoiceTerm>, String> {
        self.with_conn_reported("voice_terms.list", |conn| {
            voice_terms::list_voice_terms(conn).map_err(|error| error.to_string())
        })
        .map_err(|error| format!("voice dictionary list failed: {error}"))
    }

    /// Create a user-confirmed term and aliases in the encrypted local database.
    pub fn create_voice_term(&self, input: &NewVoiceTerm) -> Result<VoiceTerm, String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("voice_terms.create", |conn| {
            voice_terms::create_voice_term(conn, input, now).map_err(|error| error.to_string())
        })
        .map_err(|error| format!("voice dictionary create failed: {error}"))
    }

    /// Replace a user term atomically. `Ok(None)` means the requested term no longer exists.
    pub fn update_voice_term(
        &self,
        id: i64,
        input: &NewVoiceTerm,
    ) -> Result<Option<VoiceTerm>, String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("voice_terms.update", |conn| {
            voice_terms::update_voice_term(conn, id, input, now).map_err(|error| error.to_string())
        })
        .map_err(|error| format!("voice dictionary update failed: {error}"))
    }

    /// Remove one user term and its aliases.
    pub fn delete_voice_term(&self, id: i64) -> Result<bool, String> {
        self.with_conn_mut_reported("voice_terms.delete", |conn| {
            voice_terms::delete_voice_term(conn, id).map_err(|error| error.to_string())
        })
        .map_err(|error| format!("voice dictionary delete failed: {error}"))
    }

    /// Bulk-import pasted CSV / TSV / Markdown-table text as user-confirmed terms.
    /// Rows whose canonical spelling already exists in the same scope are updated; the report
    /// lists every row that did not land, with its reason. Text that parses into no rows is the
    /// caller's mistake, not a storage fault.
    pub fn import_voice_terms(
        &self,
        text: &str,
    ) -> Result<shogun_memory::voice_terms_import::ImportReport, String> {
        use shogun_memory::voice_terms_import;
        let parsed = voice_terms_import::parse_table(text);
        // "invalid …" is what the REST face classifies as the caller's 400, not a 500.
        if parsed.rows.is_empty() {
            return Err("invalid voice dictionary import: no rows found in the text".to_string());
        }
        let now = self.now_ms();
        self.with_conn_mut_reported("voice_terms.import", |conn| {
            voice_terms_import::import_voice_terms(conn, &parsed, now)
                .map_err(|error| error.to_string())
        })
        .map_err(|error| format!("voice dictionary import failed: {error}"))
    }

    /// Every persisted term as CSV, in the columns `import_voice_terms` reads back.
    pub fn export_voice_terms_csv(&self) -> Result<String, String> {
        self.list_voice_terms()
            .map(|terms| shogun_memory::voice_terms_import::export_csv(&terms))
    }
}
