//! Version-bound, disposable public progress. Terminal business state stays durable.
use crate::{engine::App, handling::Run, model::State};

impl App {
    pub(crate) fn preview_run(&self, durable: &Run) -> Run {
        let mut previews = self.handling_previews.lock().unwrap();
        if let Some(preview) = previews.get(&durable.id) {
            if !durable.stop_requested
                && preview.base_updated == durable.updated
                && preview.run.input_revision == durable.input_revision
            {
                return preview.run.clone();
            }
        }
        previews.remove(&durable.id);
        durable.clone()
    }

    pub(crate) fn live_handling(&self, state: &State, task: Option<&str>) -> Vec<Run> {
        let mut previews = self.handling_previews.lock().unwrap();
        previews.retain(|id, preview| {
            state.handling.runs.iter().any(|run| {
                run.id == *id
                    && !run.stop_requested
                    && run.updated == preview.base_updated
                    && run.input_revision == preview.run.input_revision
            })
        });
        previews
            .values()
            .filter(|p| task.is_none_or(|task| p.run.task_id == task))
            .map(|p| p.run.clone())
            .collect()
    }
}
