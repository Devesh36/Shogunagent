# V78 rollback

`rewrite_outcomes`, `rewrite_style_notes` and `rewrite_note_impressions` are additive and hold only derived rewrite-learning data. Restore the verified pre-migration database snapshot before running a binary older than V78. Do not edit refinery history or drop these tables in a live store. Wanted style notes can be exported with `shogun style-notes list` first.
