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
        !["checking", "downloading", "installing"]
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
                    && ["starting", "running", "waiting", "stopping"].contains(&run.status.as_str())
            })
    }
    pub(crate) fn save(&self, s: &State) -> Result<(), String> {
        process::check_running()?;
        let p = crate::storage::data_dir(&self.root).join("state.json");
        let tmp = crate::storage::data_dir(&self.root).join("state.next.json");
        s.work_scheduler.validate().map_err(|e| e.to_string())?;
        if s.schema_version != STATE_SCHEMA_VERSION {
            return Err("状态 schema_version 与当前程序不一致，未保存".into());
        }
        // Stream straight to the temp file: no intermediate Value or String.
        let file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, s).map_err(|e| e.to_string())?;
        writer
            .into_inner()
            .map_err(|e| e.to_string())?
            .sync_all()
            .map_err(|e| e.to_string())?;
        if p.exists() {
            fs::copy(
                &p,
                crate::storage::data_dir(&self.root).join("state.backup.json"),
            )
            .map_err(|e| e.to_string())?;
        }
        fs::rename(tmp, p).map_err(|e| e.to_string())?;
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
