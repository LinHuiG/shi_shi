//! ShiShi desktop host. On Windows the owner runs a window, tray and
//! notifications over the core runtime; elsewhere only the headless
//! `--background` owner exists, for tests and the CLI. Command words
//! (`shishi.exe tasks list`) run the console client instead.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod args;
mod cli;
mod host;
mod notices;
mod owner;
mod picker;
mod placement;
mod plugin;
mod service;
mod storage;
mod transport;
#[cfg(any(windows, test))]
mod ui_bundle;
mod updates;
#[cfg(windows)]
mod win;
mod worker;

use args::Mode;
use service::Signal;
use shishi_core::HostEvent;
use std::sync::{mpsc, Arc, Mutex};

/// Page methods the host answers itself (see contract §5).
pub const HOST_METHODS: [&str; 6] =
    ["pick", "open_data", "about_open", "update_check", "update_install", "storage_change"];

fn main() {
    let raw = std::env::args().skip(1).collect::<Vec<_>>();
    // Console commands run before anything of the desktop is set up.
    if cli::requested(&raw) {
        std::process::exit(cli::main(raw));
    }
    let mut args = match args::parse(raw) {
        Ok(args) => args,
        Err(error) => fail(&error, None, false),
    };
    let mode = std::mem::replace(&mut args.mode, Mode::Desktop);
    // The update and transfer helpers run without a console: show failures.
    let helper = matches!(mode, Mode::ApplyUpdate(_) | Mode::DataTransfer(_));
    let result = match mode {
        Mode::PluginWorker => plugin::serve(),
        Mode::AnalysisSubmit(ticket) => shishi_acp::submit::serve(&ticket).map_err(|e| e.to_string()),
        #[cfg(windows)]
        Mode::ApplyUpdate(plan) => updates::helper(&plan),
        #[cfg(windows)]
        Mode::DataTransfer(plan) => storage::transfer(&plan),
        #[cfg(not(windows))]
        Mode::ApplyUpdate(_) | Mode::DataTransfer(_) => Err("此平台不支持此操作".into()),
        Mode::Desktop => {
            desktop(args);
            Ok(())
        }
    };
    if let Err(error) = result {
        fail(&error, None, helper);
    }
}

/// The one exit for a failed start: print, record in `data` (a plain file,
/// the database may not be open), show one short message, exit 1.
pub fn fail(error: &str, data: Option<&std::path::Path>, gui: bool) -> ! {
    eprintln!("{error}");
    if let Some(data) = data {
        let _ = std::fs::create_dir_all(data);
        let _ = std::fs::write(data.join(owner::STARTUP_ERROR), error);
    }
    #[cfg(windows)]
    if gui {
        win::fatal(error);
    }
    #[cfg(not(windows))]
    let _ = gui;
    std::process::exit(1)
}

/// Own the data directory and run until told to stop; a second launch
/// hands over to the running owner and ends normally.
fn desktop(args: args::Args) {
    let gui = !args.background;
    #[cfg(not(windows))]
    if gui {
        fail("此平台只支持 --background 无界面后台", None, false);
    }
    let root = owner::root(&args).unwrap_or_else(|e| fail(&e, None, gui));
    let data = transport::data_directory(&root).unwrap_or_else(|e| fail(&e, None, gui));
    let owner = match owner::start(args, root, data.clone()) {
        Ok(Some(owner)) => owner,
        Ok(None) => return,
        Err(error) => fail(&error, Some(&data), gui),
    };
    #[cfg(windows)]
    let result = win::run(owner);
    #[cfg(not(windows))]
    let result = headless(owner);
    if let Err(error) = result {
        fail(&error, Some(&data), gui);
    }
}

enum Event {
    Signal(Signal),
    Exit { restart: bool },
}

/// The owner without a window: serve the CLI until told to stop.
#[cfg_attr(windows, allow(dead_code))]
fn headless(owner: Arc<owner::Owner>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let events = Mutex::new(tx.clone());
    owner.host.connect(move |event| {
        if let HostEvent::Exit { restart } = event {
            let _ = events.lock().unwrap().send(Event::Exit { restart });
        }
    });
    let signals = Mutex::new(tx);
    let service = owner.serve(move |signal| {
        let _ = signals.lock().unwrap().send(Event::Signal(signal));
    })?;
    let restart = loop {
        match rx.recv() {
            Ok(Event::Signal(Signal::Show | Signal::Hide)) => {}
            Ok(Event::Signal(Signal::Shutdown | Signal::Handoff)) | Err(_) => break false,
            Ok(Event::Exit { restart }) => break restart,
        }
    };
    drop(service);
    owner.shutdown();
    if restart {
        owner.relaunch()?;
    }
    Ok(())
}
