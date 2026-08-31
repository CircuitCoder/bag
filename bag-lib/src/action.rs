use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    /// Navigate to another path
    Navigate { to: String },
    /// Redirect to another path,
    Redirect { to: String },
}
