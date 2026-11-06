//! What the settings page stores besides the config file itself.

use serde_json::Value;

use super::{audit, Db};
use crate::Result;

impl Db {
    /// Audit a saved configuration; `reset` also clears the message
    /// filter's early-run hint and progress.
    pub async fn record_settings(&self, before: Value, after: Value, reset: bool) -> Result<()> {
        self.write(move |tx| {
            audit::write(tx, "settings", "", "修改设置", &before, &after)?;
            if reset {
                super::analysis::reset_filter(tx)?;
            }
            Ok(())
        })
        .await
    }
}
