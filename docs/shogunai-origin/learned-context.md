# Context learned over time

The existing nightly Batch classification now extracts named people, projects, tools,
organizations, topics, work relationships, education and observed activities alongside
commitments and open tasks. It consumes the existing source-filtered capture stream, with the
same durable cycle, retry, spend controls and exclusions. This adds no new capture mechanism,
history import, transcript upload or separate model request during typing.

Every entity must cite a literal retained source quote. Work/study records name the subject and
preserve the actual relationship wording; viewing a job ad does not establish employment.
Capture timestamps remain observation times, not proof of when an event happened or whether a
role is current. Multiple sightings retain their separate evidence. Deleting the last source
removes dependent knowledge; local export includes entities, quotes and provenance.

Wiki exposes these categories with source links. Query-relevant records can also enter normal
memory retrieval, including the prepared Option context and Scribe/chat retrieval, as bounded
attributed evidence. A source quote is not an instruction. Unsupported or no-longer-retained
quotes are excluded. The existing period summaries still handle time-based recall.

The saved About You profile remains user-controlled. Captured first-person text can belong to
someone else, so it does not automatically overwrite that profile. Explicit user notes may
become separately reviewable self facts. When writing-style learning is enabled, Scribe uses an
already-saved account name to expand first-person searches such as “my education”; no name is
guessed from browsing history. A same-name match still needs contextual evidence.

Writing personalization is separate: [recipient/app examples](writing-style-learning.md) learn
from original Option drafts and corrections. This release does not infer authorship or passive
message acceptance from arbitrary captured text, nor silently train on other people's messages.

The showcase integration includes PRs #442, #443 and #439, including dashboard activity.
Schema V35 stores writing examples, V36 scopes them by recipient, V37 stores knowledge,
and V38 stores dashboard activity. Use a fresh isolated showcase database or upgrade a
main/#442/#443 database. A database previously opened by the standalone #439 build has
different V35/V36 checksums; do not run this build against it or edit migration history.
Preserve that database and use a separate data suffix for the showcase.
