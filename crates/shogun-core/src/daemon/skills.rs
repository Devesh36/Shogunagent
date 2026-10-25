//! User-authored skill runs (issue #251): the queue every face shares.

use super::*;
use shogun_memory::skill_runs::{self, SkillRun};

impl Db {
    /// Ask for a run of `skill` at its effective `level` from `origin` (`ui|api|mcp|cli`).
    pub fn skill_run_request(
        &self,
        skill: &str,
        level: &str,
        origin: &str,
        input: &str,
    ) -> Result<SkillRun, String> {
        let now = self.now_ms();
        self.with_conn("skills.run_request", |c| {
            skill_runs::request(c, skill, level, origin, input, now)
        })
        .map_err(|fault| format!("skill run request failed: {}", fault.as_str()))
    }

    pub fn skill_run(&self, id: i64) -> Option<SkillRun> {
        self.with_conn("skills.run_get", |c| skill_runs::get(c, id)).ok().flatten()
    }

    pub fn skill_runs_recent(&self, limit: usize) -> Vec<SkillRun> {
        self.with_conn("skills.runs_recent", |c| skill_runs::recent(c, limit))
            .unwrap_or_default()
    }

    /// The user confirms or cancels a run that is waiting for them.
    pub fn skill_run_decide(&self, id: i64, confirm: bool) -> Result<bool, String> {
        let now = self.now_ms();
        self.with_conn("skills.run_decide", |c| skill_runs::decide(c, id, confirm, now))
            .map_err(|fault| format!("skill run decision failed: {}", fault.as_str()))
    }

    /// The desktop runner takes the next pending run.
    pub fn skill_run_claim_next(&self) -> Option<SkillRun> {
        let now = self.now_ms();
        self.with_conn("skills.run_claim", |c| skill_runs::claim_next(c, now))
            .ok()
            .flatten()
    }

    pub fn skill_run_finish(&self, id: i64, result: Result<&str, &str>) -> bool {
        let now = self.now_ms();
        self.with_conn("skills.run_finish", |c| skill_runs::finish(c, id, result, now))
            .unwrap_or(false)
    }

    /// Runs a crash left `running` go back to `pending` (desktop start).
    pub fn skill_runs_requeue_interrupted(&self) -> usize {
        let now = self.now_ms();
        self.with_conn("skills.run_requeue", |c| skill_runs::requeue_interrupted(c, now))
            .unwrap_or(0)
    }
}

// scheduled jobs (issue #252)

use shogun_memory::scheduled_jobs::{self, AddError, Fired, Job};

impl Db {
    pub fn schedules(&self) -> Vec<Job> {
        self.with_conn("schedules.list", scheduled_jobs::list).unwrap_or_default()
    }

    /// Add a schedule for `skill` on the `days` bitmask at local `minute_of_day`.
    pub fn schedule_add(
        &self,
        skill: &str,
        input: &str,
        days: u8,
        minute_of_day: u16,
        origin: &str,
    ) -> Result<Job, String> {
        let now = self.now_ms();
        let offset = shogun_memory::due::local_offset_secs(now);
        self.with_conn("schedules.add", |c| {
            Ok::<_, rusqlite::Error>(scheduled_jobs::add(c, skill, input, days, minute_of_day, origin, now, offset))
        })
        .map_err(|fault| format!("schedule add failed: {}", fault.as_str()))?
        .map_err(|e| match e {
            AddError::TooMany => format!("at most {} schedules", scheduled_jobs::MAX_SCHEDULES),
            AddError::Sqlite(_) => "schedule add failed".to_string(),
        })
    }

    pub fn schedule_remove(&self, id: i64) -> Result<bool, String> {
        self.with_conn("schedules.remove", |c| scheduled_jobs::remove(c, id))
            .map_err(|fault| format!("schedule remove failed: {}", fault.as_str()))
    }

    /// Enqueue a skill run for every schedule whose slot came due (the desktop runner's tick).
    pub fn schedules_tick(&self, level_of: impl Fn(&str) -> Option<String>) -> Vec<Fired> {
        let now = self.now_ms();
        let offset = shogun_memory::due::local_offset_secs(now);
        self.with_conn("schedules.tick", |c| scheduled_jobs::tick(c, now, offset, level_of))
            .unwrap_or_default()
    }
}
