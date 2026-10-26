//! Durable business-state commits.
use super::*;

impl App {
    pub fn persist(&self) -> Result<(), String> {
        let s = self.state.lock().unwrap();
        self.save(&s)
    }
    pub(crate) fn saves_revision(&self) -> u64 {
        self.saves.load(Ordering::SeqCst)
    }
    pub(crate) fn storage_ready(&self) -> bool {
        !self.tool_runtime.testing()
            && !["checking", "downloading", "installing"]
                .contains(&self.updates.snapshot()["status"].as_str().unwrap_or(""))
            && ![
                &self.busy,
                &self.ai_busy,
                &self.summary_busy,
                &self.archive_busy,
                &self.prompt_busy,
            ]
            .iter()
            .any(|flag| flag.load(Ordering::SeqCst))
            && !self.state.lock().unwrap().handling.runs.iter().any(|run| {
                !run.desktop_control
                    && run.connection.as_ref().is_none_or(|c| c.driver != "codex")
                    && !run.stop_requested
                    && crate::handling::active(run)
            })
    }
    pub(crate) fn save(&self, s: &State) -> Result<(), String> {
        process::check_running()?;
        let directory = crate::storage::data_dir(&self.root);
        let mut store = self.store.lock().unwrap();
        if store.is_none() {
            *store = Some(crate::state_store::Store::open(&directory)?);
        }
        store.as_mut().unwrap().save(s)?;
        self.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    /// Publish a business edit only after its complete candidate is durable.
    /// The closure must not reacquire state or start external work.
    pub(crate) fn commit_state<R>(
        &self,
        edit: impl FnOnce(&mut State) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        let result = edit(&mut next)?;
        self.save(&next)?;
        *locked = next;
        Ok(result)
    }
}
