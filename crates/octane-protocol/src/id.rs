//! Newtyped identifiers.
//!
//! UUIDv7 rather than v4: the timestamp prefix makes ids sort chronologically,
//! so "messages in a thread, in order" is an index scan and not a sort.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a fresh, time-ordered id.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::now_v7().simple()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(
    /// Identifies a durable conversation container.
    ThreadId, "thr"
);
id_type!(
    /// Identifies one unit of agent work within a thread.
    TurnId, "trn"
);
id_type!(
    /// Identifies an atomic unit of I/O within a turn.
    ItemId, "itm"
);
id_type!(
    /// Identifies a single tool invocation, matching call to result.
    ToolCallId, "tc"
);
id_type!(
    /// Identifies a running agent session (a thread plus its live state).
    SessionId, "ses"
);
