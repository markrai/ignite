//! Replica (installation) state. Deliberately separate from library content.
//!
//! A [`ReplicaId`] names one installation or client — a Kindle Scribe, a browser profile, a
//! future tablet or desktop app — and authors `SyncEnvelopeV1::replica_id`. It is stored in
//! application state *outside* the library (Kindle: `/mnt/us/scribe-app/state/replica.json`),
//! so copying or restoring a library never clones the replica identity. It is never derived
//! from hardware serial numbers.

use serde::{Deserialize, Serialize};

use crate::canonical::{from_json, to_canonical_json};
use crate::error::{refuse, Result};
use crate::ids::ReplicaId;

pub const REPLICA_STATE_VERSION: u32 = 1;
pub const REPLICA_STATE_FILE: &str = "replica.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaStateV1 {
    pub v: u32,
    pub replica_id: ReplicaId,
}

impl ReplicaStateV1 {
    pub fn new(replica_id: ReplicaId) -> Self {
        Self {
            v: REPLICA_STATE_VERSION,
            replica_id,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.v != REPLICA_STATE_VERSION {
            refuse!(
                "unsupported replica state version {} (need {REPLICA_STATE_VERSION})",
                self.v
            );
        }
        Ok(())
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        to_canonical_json(self)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s: Self = from_json(bytes)?;
        s.validate()?;
        Ok(s)
    }
}
