use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleHeartbeat {
    pub expiration: i32,
    pub id: String,
    pub state: String,
    pub sandbox: String,
}

impl TitleHeartbeat {
    pub fn active(title_id: impl Into<String>) -> Self {
        Self {
            expiration: 600,
            id: title_id.into(),
            state: "active".into(),
            sandbox: "RETAIL".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub titles: Vec<TitleHeartbeat>,
}

impl HeartbeatRequest {
    pub fn for_title(title_id: impl Into<String>) -> Self {
        Self {
            titles: vec![TitleHeartbeat::active(title_id)],
        }
    }

    pub fn for_titles<I, S>(title_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            titles: title_ids.into_iter().map(TitleHeartbeat::active).collect(),
        }
    }
}
