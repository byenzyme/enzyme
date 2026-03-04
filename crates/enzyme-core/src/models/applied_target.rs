use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A record of an external directory that this vault's catalysts have been applied to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedTarget {
    pub id: Option<i64>,
    pub target_path: String,
    pub doc_count: i64,
    pub catalyst_count: i64,
    pub applied_at: DateTime<Utc>,
}
