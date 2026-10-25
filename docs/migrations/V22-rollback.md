# V22 rollback

Create and verify an encrypted backup before migration. To roll back, stop the newer binary and
restore that backup. V22 only adds period-summary tables, indexes, and triggers; it does not rewrite
V21 projection or provenance tables.
