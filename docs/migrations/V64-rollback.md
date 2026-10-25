# V64 dictionary auto-add rollback

Stop the app and back up the encrypted database first. Do not edit a live database.

Before removing anything, inspect `refinery_schema_history` read-only. Its applied version 64 row must have the **name and checksum of this build's bundled `V64__dictionary_autoadd.sql`**, and there must be **no applied version greater than 64**. If either check fails, or the exact expected name/checksum cannot be established, stop: use the matching build and backup instead. A generic `DELETE ... WHERE version = 64` could corrupt a database created by an experimental build with a different V64 migration.

Only after both guards pass, a selective rollback may drop `dictionary_autoadd_corrections`, then `dictionary_autoadd_settings`, and remove that verified dictionary V64 history row. Leave every older migration row and all `voice_terms` untouched. Terms already auto-added remain ordinary editable voice terms; remove them through Settings if desired.

Experimental dictionary builds used V51, V53, V58, or V59 for different histories. Their databases require their matching builds and backups; never relabel applied history as this V64 or blindly integrate this migration into them.
