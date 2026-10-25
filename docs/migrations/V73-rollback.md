# V73 rollback

Keep the additive `evidence` column when rolling back the application. Older code ignores it. Do not edit or remove the applied migration. Restore the pre-migration database snapshot only if a full data rollback is required.
