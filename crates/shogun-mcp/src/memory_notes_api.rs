//! Local, reversible user-authored memory changes shared by every surface.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum NoteChange {
    Save {
        text: String,
    },
    Update {
        id: i64,
        revision: i64,
        text: String,
    },
    Forget {
        id: i64,
        revision: i64,
    },
    Undo {
        id: i64,
        revision: i64,
    },
}
