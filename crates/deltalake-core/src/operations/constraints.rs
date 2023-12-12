//! Add a check constraint to a table

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use datafusion::execution::context::SessionState;
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use futures::future::BoxFuture;
use futures::StreamExt;
use serde_json::json;

use crate::delta_datafusion::{register_store, DeltaDataChecker, DeltaScanBuilder};
use crate::kernel::{Action, CommitInfo, IsolationLevel, Metadata, Protocol};
use crate::logstore::LogStoreRef;
use crate::operations::datafusion_utils::Expression;
use crate::operations::transaction::commit;
use crate::protocol::DeltaOperation;
use crate::table::state::DeltaTableState;
use crate::table::Constraint;
use crate::DeltaTable;
use crate::{DeltaResult, DeltaTableError};

/// Build a constraint to add to a table
pub struct ConstraintBuilder {
    snapshot: DeltaTableState,
    name: Option<String>,
    expr: Option<Expression>,
    log_store: LogStoreRef,
    state: Option<SessionState>,
}

impl ConstraintBuilder {
    /// Create a new builder
    pub fn new(log_store: LogStoreRef, snapshot: DeltaTableState) -> Self {
        Self {
            name: None,
            expr: None,
            snapshot,
            log_store,
            state: None,
        }
    }

    /// Specify the constraint to be added
    pub fn with_constraint<S: Into<String>, E: Into<Expression>>(
        mut self,
        column: S,
        expression: E,
    ) -> Self {
        self.name = Some(column.into());
        self.expr = Some(expression.into());
        self
    }

    /// Specify the datafusion session context
    pub fn with_session_state(mut self, state: SessionState) -> Self {
        self.state = Some(state);
        self
    }
}

impl std::future::IntoFuture for ConstraintBuilder {
    type Output = DeltaResult<DeltaTable>;

    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let mut this = self;

        Box::pin(async move {
            let name = match this.name {
                Some(v) => v,
                None => return Err(DeltaTableError::Generic("No name provided".to_string())),
            };
            let expr = match this.expr {
                Some(Expression::String(s)) => s,
                Some(Expression::DataFusion(e)) => e.to_string(),
                None => {
                    return Err(DeltaTableError::Generic(
                        "No expression provided".to_string(),
                    ))
                }
            };

            let mut metadata = this
                .snapshot
                .metadata()
                .ok_or(DeltaTableError::NoMetadata)?
                .clone();
            let configuration_key = format!("delta.constraints.{}", name);

            if metadata.configuration.contains_key(&configuration_key) {
                return Err(DeltaTableError::Generic(format!(
                    "Constraint with name: {} already exists, expr: {}",
                    name, expr
                )));
            }

            let state = this.state.unwrap_or_else(|| {
                let session = SessionContext::new();
                register_store(this.log_store.clone(), session.runtime_env());
                session.state()
            });

            // Checker built here with the one time constraint to check.
            let checker = DeltaDataChecker::new_with_constraints(vec![Constraint::new("*", &expr)]);
            let scan = DeltaScanBuilder::new(&this.snapshot, this.log_store.clone(), &state)
                .build()
                .await?;

            let plan: Arc<dyn ExecutionPlan> = Arc::new(scan);
            let mut tasks = vec![];
            for p in 0..plan.output_partitioning().partition_count() {
                let inner_plan = plan.clone();
                let inner_checker = checker.clone();
                let task_ctx = Arc::new(TaskContext::from(&state));
                let mut record_stream: SendableRecordBatchStream =
                    inner_plan.execute(p, task_ctx)?;
                let handle: tokio::task::JoinHandle<DeltaResult<()>> =
                    tokio::task::spawn(async move {
                        while let Some(maybe_batch) = record_stream.next().await {
                            let batch = maybe_batch?;
                            inner_checker.check_batch(&batch).await?;
                        }
                        Ok(())
                    });
                tasks.push(handle);
            }
            futures::future::join_all(tasks)
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| DeltaTableError::Generic(err.to_string()))?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            // We have validated the table passes it's constraints, now to add the constraint to
            // the table.

            metadata
                .configuration
                .insert(format!("delta.constraints.{}", name), Some(expr.clone()));

            let old_protocol = this.snapshot.protocol();
            let protocol = Protocol {
                min_reader_version: if old_protocol.min_reader_version > 1 {
                    old_protocol.min_reader_version
                } else {
                    1
                },
                min_writer_version: if old_protocol.min_writer_version > 3 {
                    old_protocol.min_writer_version
                } else {
                    3
                },
                reader_features: old_protocol.reader_features.clone(),
                writer_features: old_protocol.writer_features.clone(),
            };

            let operational_parameters = HashMap::from_iter([
                ("name".to_string(), json!(&name)),
                ("expr".to_string(), json!(&expr)),
            ]);

            let operations = DeltaOperation::AddConstraint {
                name: name.clone(),
                expr: expr.clone(),
            };

            let commit_info = CommitInfo {
                timestamp: Some(Utc::now().timestamp_millis()),
                operation: Some(operations.name().to_string()),
                operation_parameters: Some(operational_parameters),
                read_version: Some(this.snapshot.version()),
                isolation_level: Some(IsolationLevel::Serializable),
                is_blind_append: Some(false),
                ..Default::default()
            };

            let actions = vec![
                Action::CommitInfo(commit_info),
                Action::Metadata(Metadata::try_from(metadata)?),
                Action::Protocol(protocol),
            ];

            let version = commit(
                this.log_store.as_ref(),
                &actions,
                operations,
                &this.snapshot,
                None,
            )
            .await?;

            this.snapshot
                .merge(DeltaTableState::from_actions(actions, version)?, true, true);
            Ok(DeltaTable::new_with_state(this.log_store, this.snapshot))
        })
    }
}

#[cfg(feature = "datafusion")]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Array, Int32Array, RecordBatch, StringArray};

    use crate::writer::test_utils::{create_bare_table, get_arrow_schema, get_record_batch};
    use crate::{DeltaOps, DeltaResult};

    #[cfg(feature = "datafusion")]
    #[tokio::test]
    async fn add_constraint_with_invalid_data() -> DeltaResult<()> {
        let batch = get_record_batch(None, false);
        let write = DeltaOps(create_bare_table())
            .write(vec![batch.clone()])
            .await?;
        let table = DeltaOps(write);

        let constraint = table
            .add_constraint()
            .with_constraint("id", "value > 5")
            .await;
        dbg!(&constraint);
        assert!(constraint.is_err());
        Ok(())
    }

    #[cfg(feature = "datafusion")]
    #[tokio::test]
    async fn add_valid_constraint() -> DeltaResult<()> {
        let batch = get_record_batch(None, false);
        let write = DeltaOps(create_bare_table())
            .write(vec![batch.clone()])
            .await?;
        let table = DeltaOps(write);

        let constraint = table
            .add_constraint()
            .with_constraint("id", "value < 1000")
            .await;
        dbg!(&constraint);
        assert!(constraint.is_ok());
        let version = constraint?.version();
        assert_eq!(version, 1);
        Ok(())
    }

    #[cfg(feature = "datafusion")]
    #[tokio::test]
    async fn add_conflicting_named_constraint() -> DeltaResult<()> {
        let batch = get_record_batch(None, false);
        let write = DeltaOps(create_bare_table())
            .write(vec![batch.clone()])
            .await?;
        let table = DeltaOps(write);

        let new_table = table
            .add_constraint()
            .with_constraint("id", "value < 60")
            .await?;

        let new_table = DeltaOps(new_table);
        let second_constraint = new_table
            .add_constraint()
            .with_constraint("id", "value < 10")
            .await;
        dbg!(&second_constraint);
        assert!(second_constraint.is_err());
        Ok(())
    }

    #[cfg(feature = "datafusion")]
    #[tokio::test]
    async fn write_data_that_violates_constraint() -> DeltaResult<()> {
        let batch = get_record_batch(None, false);
        let write = DeltaOps(create_bare_table())
            .write(vec![batch.clone()])
            .await?;

        let table = DeltaOps(write)
            .add_constraint()
            .with_constraint("id", "value > 0")
            .await?;
        let table = DeltaOps(table);
        let invalid_values: Vec<Arc<dyn Array>> = vec![
            Arc::new(StringArray::from(vec!["A"])),
            Arc::new(Int32Array::from(vec![-10])),
            Arc::new(StringArray::from(vec!["2021-02-02"])),
        ];
        let batch = RecordBatch::try_new(get_arrow_schema(&None), invalid_values)?;
        let err = table.write(vec![batch]).await;
        dbg!(&err);
        assert!(err.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn write_data_that_does_not_violate_constraint() -> DeltaResult<()> {
        let batch = get_record_batch(None, false);
        let write = DeltaOps(create_bare_table())
            .write(vec![batch.clone()])
            .await?;
        let table = DeltaOps(write);

        let err = table.write(vec![batch]).await;

        assert!(err.is_ok());
        Ok(())
    }
}