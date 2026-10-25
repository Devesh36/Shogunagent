# V79 rollback

Data-only: sets `writing_voice_settings.enabled = 1` for rows that never answered the offer. No schema change, so an older binary opens the database as is; sharing then follows `enabled` as before. To go back to off, the user can switch "Use my writing style" off in Settings.
