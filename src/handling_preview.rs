//! Version-bound, disposable public progress. Terminal business state stays durable.
use crate::{engine::App, handling::Run, model::State};
use serde_json::{json, Value};

fn valid(durable: &Run, preview: &crate::handling::Preview) -> bool {
    !durable.stop_requested
        && preview.base_updated == durable.updated
        && preview.run.input_revision == durable.input_revision
}

fn view(run: &Run, base: i64, revision: u64, full: bool) -> Value {
    let mut value = if full {
        json!(run)
    } else {
        json!({"id":run.id,"task_id":run.task_id,"status":run.status,
            "desktop_control":run.desktop_control,"desktop_status":run.desktop_status,
            "transport":run.transport,"waiting_on":run.waiting_on,
            "thread_id":run.thread_id,"stop_requested":run.stop_requested,
            "project_name":run.project_name,"updated":run.updated})
    };
    value["_details"] = json!(full);
    value["_base_updated"] = json!(base);
    value["_stream_revision"] = json!([run.updated, revision]);
    value
}

impl App {
    pub(crate) fn preview_run(&self, durable: &Run) -> Run {
        let mut previews = self.handling_previews.lock().unwrap();
        if let Some(preview) = previews.get(&durable.id) {
            if valid(durable, preview) {
                return preview.run.clone();
            }
        }
        previews.remove(&durable.id);
        durable.clone()
    }

    pub(crate) fn handling_view(&self, durable: &Run, full: bool) -> Value {
        let mut previews = self.handling_previews.lock().unwrap();
        if let Some(preview) = previews.get(&durable.id) {
            if valid(durable, preview) {
                return view(&preview.run, durable.updated, preview.revision, full);
            }
        }
        previews.remove(&durable.id);
        view(durable, durable.updated, 0, full)
    }

    pub(crate) fn live_handling(
        &self,
        state: &State,
        task: Option<&str>,
        selected: Option<&str>,
    ) -> Vec<Value> {
        let mut previews = self.handling_previews.lock().unwrap();
        previews.retain(|id, preview| {
            state
                .handling
                .runs
                .iter()
                .any(|run| run.id == *id && valid(run, preview))
        });
        previews
            .values()
            .filter(|p| {
                task == Some(p.run.task_id.as_str()) || selected == Some(p.run.task_id.as_str())
            })
            .map(|p| {
                let mut header = p.run.clone();
                header.rounds = Default::default();
                let mut value = view(&header, p.base_updated, p.revision, true);
                value.as_object_mut().unwrap().remove("rounds");
                value["_round_count"] = json!(p.run.rounds.len());
                // Keep every round touched in this durable generation. A later
                // preview can restore its durable text after the client has
                // already received a different preview.
                value["_round_changes"] = json!(p
                    .changed_rounds
                    .iter()
                    .filter_map(|index| p.run.rounds.get(*index).map(|round| (*index, round)))
                    .collect::<Vec<_>>());
                value
            })
            .collect()
    }
}
