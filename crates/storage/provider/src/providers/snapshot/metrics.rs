use std::{collections::HashMap, time::Duration};

use itertools::Itertools;
use metrics::{Counter, Histogram};
use reth_metrics::Metrics;
use reth_primitives::SnapshotSegment;
use strum::{EnumIter, IntoEnumIterator};

/// Metrics for the snapshot provider.
#[derive(Debug)]
pub struct SnapshotProviderMetrics {
    segment_operations:
        HashMap<(SnapshotSegment, SnapshotProviderOperation), SnapshotProviderOperationMetrics>,
}

impl Default for SnapshotProviderMetrics {
    fn default() -> Self {
        Self {
            segment_operations: SnapshotSegment::iter()
                .cartesian_product(SnapshotProviderOperation::iter())
                .map(|(segment, operation)| {
                    (
                        (segment, operation),
                        SnapshotProviderOperationMetrics::new_with_labels(&[
                            ("segment", segment.as_str()),
                            ("operation", operation.as_str()),
                        ]),
                    )
                })
                .collect(),
        }
    }
}

impl SnapshotProviderMetrics {
    pub(crate) fn record_segment_operation(
        &self,
        segment: SnapshotSegment,
        operation: SnapshotProviderOperation,
        duration: Option<Duration>,
    ) {
        self.segment_operations
            .get(&(segment, operation))
            .expect("segment operation metrics should exist")
            .calls_total
            .increment(1);

        if let Some(duration) = duration {
            self.segment_operations
                .get(&(segment, operation))
                .expect("segment operation metrics should exist")
                .write_duration_seconds
                .record(duration.as_secs_f64());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub(crate) enum SnapshotProviderOperation {
    InitCursor,
    OpenWriter,
    Append,
    Prune,
    IncrementBlock,
    CommitWriter,
}

impl SnapshotProviderOperation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::InitCursor => "init-cursor",
            Self::OpenWriter => "open-writer",
            Self::Append => "append",
            Self::Prune => "prune",
            Self::IncrementBlock => "increment-block",
            Self::CommitWriter => "commit-writer",
        }
    }
}

#[derive(Metrics)]
#[metrics(scope = "snapshots.jar_provider")]
pub(crate) struct SnapshotProviderOperationMetrics {
    /// Total number of snapshot jar provider operations made.
    calls_total: Counter,
    /// The time it took to execute the snapshot jar provider operation that writes data.
    write_duration_seconds: Histogram,
}
