//! `shishi_core::Host` for this program: events go to whichever loop owns
//! the process (window or headless); plugins run in the worker process.
use serde_json::Value;
use shishi_core::{Error, Host, HostEvent, Result};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, Mutex},
};

type Deliver = Box<dyn Fn(HostEvent) + Send + Sync>;

#[derive(Default)]
struct Sink {
    deliver: Option<Deliver>,
    /// Events raised before the loop connected, delivered on connect.
    pending: Vec<HostEvent>,
}

pub struct AppHost {
    #[cfg_attr(not(windows), allow(dead_code))]
    data: PathBuf,
    root: PathBuf,
    sink: Mutex<Sink>,
    plugins: crate::worker::Client,
}

impl AppHost {
    pub fn new(root: PathBuf, data: PathBuf) -> Arc<Self> {
        let plugins = crate::worker::Client::default();
        Arc::new(Self { data, root, sink: Mutex::default(), plugins })
    }

    pub fn connect(&self, deliver: impl Fn(HostEvent) + Send + Sync + 'static) {
        let mut sink = self.sink.lock().unwrap();
        for event in sink.pending.drain(..) {
            deliver(event);
        }
        sink.deliver = Some(Box::new(deliver));
    }
}

impl Host for AppHost {
    fn install_root(&self) -> PathBuf {
        self.root.clone()
    }

    fn event(&self, event: HostEvent) {
        let mut sink = self.sink.lock().unwrap();
        match &sink.deliver {
            Some(deliver) => deliver(event),
            None => sink.pending.push(event),
        }
    }

    #[cfg(windows)]
    fn image_preview(&self, path: &Path) -> Result<String> {
        crate::win::preview::decode(&self.data, path).map_err(Error)
    }

    fn plugin_call(
        &self,
        manifest: &Path,
        settings: bool,
        input: Value,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Value> {
        self.plugins.call(manifest, settings, input, cancel.as_deref()).map_err(Error)
    }

    fn plugin_reset(&self) -> Result<()> {
        self.plugins.reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
