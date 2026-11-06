//! Pending task notices behind the toast windows. Every change is its own
//! notice, newest first, living five minutes from first display. Expiry is
//! a deadline for the window loop, never a poll.
#![cfg_attr(not(windows), allow(dead_code))]
use serde::Serialize;
use shishi_core::HostEvent;
use std::time::{Duration, Instant};

pub const LIFETIME: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Notice {
    pub id: String,
    pub title: String,
    pub summary: String,
    /// Clicking opens this task; otherwise it only shows the main window.
    #[serde(skip)]
    pub task: Option<String>,
}

impl Notice {
    pub fn from_event(event: &HostEvent) -> Option<Self> {
        let HostEvent::Notify { title, body, task_id } = event else { return None };
        Some(Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.clone(),
            summary: body.clone(),
            task: task_id.clone(),
        })
    }
}

struct Pending {
    notice: Notice,
    shown_at: Option<Instant>,
}

#[derive(Default)]
pub struct Queue {
    items: Vec<Pending>,
}

impl Queue {
    pub fn push(&mut self, notice: Notice) {
        self.items.insert(0, Pending { notice, shown_at: None });
    }

    /// Drop expired notices and return the first `capacity` to display.
    pub fn active(&mut self, at: Instant, capacity: usize) -> Vec<Notice> {
        self.items.retain(|i| i.shown_at.is_none_or(|shown| at.saturating_duration_since(shown) < LIFETIME));
        self.items.iter().take(capacity).map(|i| i.notice.clone()).collect()
    }

    pub fn shown(&mut self, id: &str, at: Instant) {
        if let Some(i) = self.items.iter_mut().find(|i| i.notice.id == id) {
            i.shown_at.get_or_insert(at);
        }
    }

    pub fn close(&mut self, id: &str) {
        self.items.retain(|i| i.notice.id != id);
    }

    /// Drop every notice of a task the user has opened.
    pub fn dismiss_task(&mut self, task: &str) {
        self.items.retain(|i| i.notice.task.as_deref() != Some(task));
    }

    pub fn dismiss(&mut self) {
        self.items.clear();
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    /// When the earliest displayed notice expires.
    pub fn deadline(&self) -> Option<Instant> {
        self.items.iter().filter_map(|i| i.shown_at).min().map(|at| at + LIFETIME)
    }
}

#[cfg(test)]
mod tests;
