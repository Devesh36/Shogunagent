# V76 rollback

User memory notes and their versions are additive. Restore the verified pre-migration database snapshot before running a binary older than V76. Do not edit refinery history or drop these tables in a live store. Export wanted notes through `memory.notes.list` before restoring the snapshot.
