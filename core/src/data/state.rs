//! The read behind the `state` snapshot: each domain contributes its keys
//! through a [`Part`] inside one read, so a snapshot is always a consistent
//! view of one committed revision.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::Connection;
use serde_json::{Map, Value};

use super::{sources, task_query, Db};
use crate::state::Scope;
use crate::{Config, Result};

/// A domain's contribution to the snapshot.
pub type Part = fn(&Connection, &Scope, &mut Map<String, Value>) -> Result<()>;

/// Contributors in key order. Domains register here.
const PARTS: &[Part] = &[
    super::ai::part,
    sources::part,
    task_query::part,
    super::tools::part,
    super::prompts::part,
    super::archive::part,
    super::handling::part,
];

impl Db {
    /// The newest `shown` log rows and, unless `unchanged`, every domain's
    /// snapshot keys for `scope`.
    pub async fn snapshot(
        &self,
        scope: Scope,
        unchanged: bool,
        config: Arc<Config>,
        analyzing: BTreeMap<String, String>,
        shown: usize,
    ) -> Result<(Vec<Value>, Option<Map<String, Value>>)> {
        self.read(move |conn| {
            let activity = super::activity::recent(conn, shown)?;
            if unchanged {
                return Ok((activity, None));
            }
            let mut out = Map::new();
            for part in PARTS {
                part(conn, &scope, &mut out)?;
            }
            // Analysis keys depend on the configured window and the batch.
            super::analysis::part(conn, &config, &analyzing, &mut out)?;
            super::ai::work_part(conn, &config, &mut out)?;
            Ok((activity, Some(out)))
        })
        .await
    }
}
