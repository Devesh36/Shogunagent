# V35 writing voice rollback

Disable Learn my writing style before downgrading. Back up the encrypted memory database.
The migration is additive; older binaries may leave the three writing_voice tables intact.
To remove learned data, use Clear learned writing in Settings before downgrading. Do not
delete or alter refinery migration history. Restoring the pre-migration encrypted backup is
the supported full schema rollback.

Automatic pre-upgrade copies exist only within the configured [backup rollback window](verified-encrypted-memory-backups.md#rollback). Preserve the required copy outside app-managed rotation before rollback.
