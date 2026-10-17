//! Session-local stroke-construction vocabulary (not persisted).
//!
//! [`NotebookOp`] is **not** the durable cross-replica log. That is `SyncOperationV1` in
//! `sync-protocol` (`docs/SYNC_ARCHITECTURE.md`). These variants exist so `StrokeBuilder`,
//! `scribe-app` `PageSession`, and `ink-probe` can keep a RAM stream consistent with wet-ink
//! construction. Do not persist this enum, do not treat it as an oplog to finish, and do not
//! encode selection/move/lasso history as `NotebookOp`.
//!
//! Every [`Point`] carried here is in **canonical page coordinates** (see [`crate::coords`]),
//! never display pixels.

use crate::ids::StrokeId;
use crate::point::Point;

/// Session-local operations for wet-ink construction.
///
/// Producers that keep a RAM stream (ink-probe, `PageSession`) must stay consistent with
/// `StrokeBuilder`: a stroke the builder started must have a `BeginStroke`, and one it
/// finished (including implicitly, via a duplicate pen-down) must have a `FinishStroke`.
#[derive(Clone, Debug, PartialEq)]
pub enum NotebookOp {
    CreateNotebook {
        notebook_id: String,
    },
    CreatePage {
        notebook_id: String,
        page_id: String,
    },
    BeginStroke {
        page_id: String,
        stroke_id: StrokeId,
        point: Point,
    },
    AppendStrokePoints {
        stroke_id: StrokeId,
        points: Vec<Point>,
    },
    FinishStroke {
        stroke_id: StrokeId,
    },
    /// Abandon an in-progress stroke after an input stream gap. Not a normal pen-up.
    CancelStroke {
        stroke_id: StrokeId,
    },
    DeleteStroke {
        stroke_id: StrokeId,
    },
    DeletePage {
        page_id: String,
    },
}

#[cfg(all(test, feature = "mint"))]
mod tests {
    use super::*;
    use crate::ids::new_stroke_id;

    #[test]
    fn ops_carry_stable_stroke_id() {
        let id = new_stroke_id();
        let op = NotebookOp::FinishStroke {
            stroke_id: id.clone(),
        };
        match op {
            NotebookOp::FinishStroke { stroke_id } => assert_eq!(stroke_id, id),
            _ => panic!("unexpected"),
        }
    }
}
