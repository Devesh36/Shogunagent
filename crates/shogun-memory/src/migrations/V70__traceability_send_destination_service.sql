-- Rollback: docs/migrations/V70-rollback.md
-- V70: a confirmed send's traceability row names the service, not the recipient or title
-- (issue #924, FR-TR-01: destination = API category).
--
-- Before this, `send_exec::trace_for_send` copied the approval card's destination into the row:
-- an email's recipient address, a calendar event's or document's title, a Slack channel, an issue
-- reference. Rows live for a year (TRACEABILITY_RETENTION_MS). New rows record the service id
-- (`Service::source_str`); this rewrites the destination of earlier send rows the same way.
--
-- Only rows with an `integration.*` purpose are touched; every other column (ts, route, purpose,
-- chunk size, chunk fingerprint, third-party flag, kind) is kept. The purpose fixes the service:
-- `send_bridge::service_op` has mapped each send action to the same service since sends were
-- traced. Reads, drafts, model calls and every other row are left alone. No row is deleted.

UPDATE traceability_log
   SET destination = CASE purpose
           WHEN 'integration.send_email'            THEN 'gmail'
           WHEN 'integration.post_message'          THEN 'slack'
           WHEN 'integration.add_reaction'          THEN 'slack'
           WHEN 'integration.create_calendar_event' THEN 'gcal'
           WHEN 'integration.update_calendar_event' THEN 'gcal'
           WHEN 'integration.post_comment'          THEN 'github'
           WHEN 'integration.create_document'       THEN 'gdrive'
           WHEN 'integration.update_document'       THEN 'notion'
           WHEN 'integration.change_issue_status'   THEN 'linear'
       END
 WHERE purpose IN (
           'integration.send_email',
           'integration.post_message',
           'integration.add_reaction',
           'integration.create_calendar_event',
           'integration.update_calendar_event',
           'integration.post_comment',
           'integration.create_document',
           'integration.update_document',
           'integration.change_issue_status'
       );
