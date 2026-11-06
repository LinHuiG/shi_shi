//! Scripted agents: each run reports the queued session id and outcome.

use std::sync::Mutex;

use super::*;

#[derive(Default)]
pub struct Fake {
    /// Per run: the session id it reports, then the outcome (`Err` text fails it).
    pub script: Mutex<Vec<(&'static str, std::result::Result<&'static str, String>)>>,
    /// The turns run: text, cwd, model and effort.
    pub turns: Mutex<Vec<(String, String, String, String)>>,
    pub opened: Mutex<Vec<String>>,
    /// A run blocks until released when set.
    pub hold: Mutex<Option<mpsc::Receiver<()>>>,
}

pub struct Backend(pub std::sync::Arc<Fake>);

impl Backends for Backend {
    fn agent(&self, _: &Connection, _: &Path) -> Result<Box<dyn Agent>> {
        Ok(Box::new(Handle(self.0.clone())))
    }
    fn projects(&self, driver: &str) -> Result<Vec<Value>> {
        Ok(vec![
            json!({"id": format!("{driver}:p"), "name": format!("{driver} 项目"), "cwd": format!("/synthetic/{driver}")}),
        ])
    }
    fn open(&self, _: &Connection, thread: &str) -> Result<()> {
        self.0.opened.lock().unwrap().push(thread.into());
        Ok(())
    }
}

struct Handle(std::sync::Arc<Fake>);

impl Agent for Handle {
    fn run(&self, turn: &Turn, thread: &mut dyn FnMut(String)) -> Result<Outcome> {
        let f = &self.0;
        let cwd = turn.cwd.to_string_lossy().into_owned();
        f.turns.lock().unwrap().push((turn.text.clone(), cwd, turn.model.clone(), turn.effort.clone()));
        let (id, outcome) = f.script.lock().unwrap().remove(0);
        if !id.is_empty() {
            thread(id.into());
        }
        if let Some(hold) = f.hold.lock().unwrap().take() {
            let _ = hold.recv();
        }
        Ok(Outcome {
            status: outcome.map_err(Error)?,
            conclusion: "结论".into(),
            usage: json!({"input_tokens": 3, "output_tokens": 2}),
        })
    }
}
