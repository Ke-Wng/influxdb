//! module for query executor
use crate::system_tables::{SystemSchemaProvider, SYSTEM_SCHEMA_NAME};
use crate::{query_planner::Planner, system_tables::AllSystemSchemaTablesProvider};
use arrow::array::{ArrayRef, Int64Builder, StringBuilder, StructArray};
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use arrow_array::{Array, BooleanArray};
use async_trait::async_trait;
use data_types::NamespaceId;
use datafusion::catalog::{CatalogProvider, SchemaProvider, Session};
use datafusion::common::arrow::array::StringArray;
use datafusion::common::arrow::datatypes::{DataType, Field, Schema as DatafusionSchema};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use datafusion_util::config::DEFAULT_SCHEMA;
use datafusion_util::MemoryStream;
use influxdb3_cache::distinct_cache::{DistinctCacheFunction, DISTINCT_CACHE_UDTF_NAME};
use influxdb3_cache::last_cache::{LastCacheFunction, LAST_CACHE_UDTF_NAME};
use influxdb3_catalog::catalog::{Catalog, DatabaseSchema};
use influxdb3_internal_api::query_executor::{QueryExecutor, QueryExecutorError, QueryKind};
use influxdb3_sys_events::SysEventStore;
use influxdb3_telemetry::store::TelemetryStore;
use influxdb3_write::WriteBuffer;
use iox_query::exec::{Executor, IOxSessionContext, QueryConfig};
use iox_query::provider::ProviderBuilder;
use iox_query::query_log::QueryLog;
use iox_query::query_log::QueryText;
use iox_query::query_log::StateReceived;
use iox_query::query_log::{QueryCompletedToken, QueryLogEntries};
use iox_query::QueryDatabase;
use iox_query::{QueryChunk, QueryNamespace};
use iox_query_params::StatementParams;
use metric::Registry;
use observability_deps::tracing::{debug, info};
use schema::Schema;
use std::any::Any;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::Semaphore;
use trace::ctx::SpanContext;
use trace::span::{Span, SpanExt, SpanRecorder};
use trace_http::ctx::RequestLogContext;
use tracker::{
    AsyncSemaphoreMetrics, InstrumentedAsyncOwnedSemaphorePermit, InstrumentedAsyncSemaphore,
};

#[derive(Debug, Clone)]
pub struct QueryExecutorImpl {
    catalog: Arc<Catalog>,
    write_buffer: Arc<dyn WriteBuffer>,
    exec: Arc<Executor>,
    datafusion_config: Arc<HashMap<String, String>>,
    query_execution_semaphore: Arc<InstrumentedAsyncSemaphore>,
    query_log: Arc<QueryLog>,
    telemetry_store: Arc<TelemetryStore>,
    sys_events_store: Arc<SysEventStore>,
}

/// Arguments for [`QueryExecutorImpl::new`]
#[derive(Debug)]
pub struct CreateQueryExecutorArgs {
    pub catalog: Arc<Catalog>,
    pub write_buffer: Arc<dyn WriteBuffer>,
    pub exec: Arc<Executor>,
    pub metrics: Arc<Registry>,
    pub datafusion_config: Arc<HashMap<String, String>>,
    pub query_log_size: usize,
    pub telemetry_store: Arc<TelemetryStore>,
    pub sys_events_store: Arc<SysEventStore>,
}

impl QueryExecutorImpl {
    pub fn new(
        CreateQueryExecutorArgs {
            catalog,
            write_buffer,
            exec,
            metrics,
            datafusion_config,
            query_log_size,
            telemetry_store,
            sys_events_store,
        }: CreateQueryExecutorArgs,
    ) -> Self {
        let semaphore_metrics = Arc::new(AsyncSemaphoreMetrics::new(
            &metrics,
            &[("semaphore", "query_execution")],
        ));
        let query_execution_semaphore =
            Arc::new(semaphore_metrics.new_semaphore(Semaphore::MAX_PERMITS));
        let query_log = Arc::new(QueryLog::new(
            query_log_size,
            Arc::new(iox_time::SystemProvider::new()),
        ));
        Self {
            catalog,
            write_buffer,
            exec,
            datafusion_config,
            query_execution_semaphore,
            query_log,
            telemetry_store,
            sys_events_store,
        }
    }
}

#[async_trait]
impl QueryExecutor for QueryExecutorImpl {
    async fn query(
        &self,
        database: &str,
        query: &str,
        params: Option<StatementParams>,
        kind: QueryKind,
        span_ctx: Option<SpanContext>,
        external_span_ctx: Option<RequestLogContext>,
    ) -> Result<SendableRecordBatchStream, QueryExecutorError> {
        info!(%database, %query, ?params, ?kind, "QueryExecutorImpl as QueryExecutor::query");
        let db = self
            .namespace(database, span_ctx.child_span("get database"), false)
            .await
            .map_err(|_| QueryExecutorError::DatabaseNotFound {
                db_name: database.to_string(),
            })?
            .ok_or_else(|| QueryExecutorError::DatabaseNotFound {
                db_name: database.to_string(),
            })?;

        let params = params.unwrap_or_default();

        let token = db.record_query(
            external_span_ctx.as_ref().map(RequestLogContext::ctx),
            kind.query_type(),
            Box::new(query.to_string()),
            params.clone(),
        );

        // NOTE - we use the default query configuration on the IOxSessionContext here:
        let ctx = db.new_query_context(span_ctx, Default::default());
        let planner = Planner::new(&ctx);
        let query = query.to_string();

        // Perform query planning on a separate threadpool than the IO runtime that is servicing
        // this request by using `IOxSessionContext::run`.
        let plan = ctx
            .run(async move {
                match kind {
                    QueryKind::Sql => planner.sql(query, params).await,
                    QueryKind::InfluxQl => planner.influxql(query, params).await,
                }
            })
            .await;

        let plan = match plan.map_err(QueryExecutorError::QueryPlanning) {
            Ok(plan) => plan,
            Err(e) => {
                token.fail();
                return Err(e);
            }
        };
        let token = token.planned(&ctx, Arc::clone(&plan));

        // TODO: Enforce concurrency limit here
        let token = token.permit();

        self.telemetry_store.update_num_queries();

        match ctx.execute_stream(Arc::clone(&plan)).await {
            Ok(query_results) => {
                token.success();
                Ok(query_results)
            }
            Err(err) => {
                token.fail();
                Err(QueryExecutorError::ExecuteStream(err))
            }
        }
    }

    fn show_databases(
        &self,
        include_deleted: bool,
    ) -> Result<SendableRecordBatchStream, QueryExecutorError> {
        let mut databases = self.catalog.list_db_schema();
        // sort them to ensure consistent order, first by deleted, then by name:
        databases.sort_unstable_by(|a, b| match a.deleted.cmp(&b.deleted) {
            Ordering::Equal => a.name.cmp(&b.name),
            ordering => ordering,
        });
        if !include_deleted {
            databases.retain(|db| !db.deleted);
        }
        let mut fields = Vec::with_capacity(2);
        fields.push(Field::new("iox::database", DataType::Utf8, false));
        let mut arrays = Vec::with_capacity(2);
        let names: StringArray = databases
            .iter()
            .map(|db| db.name.as_ref())
            .collect::<Vec<&str>>()
            .into();
        let names = Arc::new(names) as Arc<dyn Array>;
        arrays.push(names);
        if include_deleted {
            fields.push(Field::new("deleted", DataType::Boolean, false));
            let deleted: BooleanArray = databases
                .iter()
                .map(|db| db.deleted)
                .collect::<Vec<bool>>()
                .into();
            let deleted = Arc::new(deleted);
            arrays.push(deleted);
        }
        let schema = DatafusionSchema::new(fields);
        let batch = RecordBatch::try_new(Arc::new(schema), arrays)
            .map_err(QueryExecutorError::DatabasesToRecordBatch)?;
        Ok(Box::pin(MemoryStream::new(vec![batch])))
    }

    async fn show_retention_policies(
        &self,
        database: Option<&str>,
        span_ctx: Option<SpanContext>,
    ) -> Result<SendableRecordBatchStream, QueryExecutorError> {
        let mut databases = if let Some(db) = database {
            vec![db.to_owned()]
        } else {
            self.catalog.db_names()
        };
        // sort them to ensure consistent order:
        databases.sort_unstable();

        let mut rows = Vec::with_capacity(databases.len());
        for database in databases {
            let db = self
                .namespace(&database, span_ctx.child_span("get database"), false)
                .await
                .map_err(|_| QueryExecutorError::DatabaseNotFound {
                    db_name: database.to_string(),
                })?
                .ok_or_else(|| QueryExecutorError::DatabaseNotFound {
                    db_name: database.to_string(),
                })?;
            let duration = db.retention_time_ns();
            let (db_name, rp_name) = split_database_name(&database);
            rows.push(RetentionPolicyRow {
                database: db_name,
                name: rp_name,
                duration,
            });
        }

        let batch = retention_policy_rows_to_batch(&rows);
        Ok(Box::pin(MemoryStream::new(vec![batch])))
    }

    fn upcast(&self) -> Arc<(dyn QueryDatabase + 'static)> {
        // NB: This clone is required to get compiler to be happy
        //     to convert `self` to dyn QueryDatabase. This wasn't
        //     possible without getting owned value of self.
        //     TODO: see if this clone can be removed satisfying
        //           grpc setup in `make_flight_server`
        let cloned_self = (*self).clone();
        Arc::new(cloned_self) as _
    }
}

#[derive(Debug)]
struct RetentionPolicyRow {
    database: String,
    name: String,
    duration: Option<i64>,
}

#[derive(Debug, Default)]
struct RetentionPolicyRowBuilder {
    database: StringBuilder,
    name: StringBuilder,
    duration: Int64Builder,
}

impl RetentionPolicyRowBuilder {
    fn append(&mut self, row: &RetentionPolicyRow) {
        self.database.append_value(row.database.as_str());
        self.name.append_value(row.name.as_str());
        self.duration.append_option(row.duration);
    }

    // Note: may be able to use something simpler than StructArray here, this is just based
    // directly on the arrow docs: https://docs.rs/arrow/latest/arrow/array/builder/index.html
    fn finish(&mut self) -> StructArray {
        StructArray::from(vec![
            (
                Arc::new(Field::new("iox::database", DataType::Utf8, false)),
                Arc::new(self.database.finish()) as ArrayRef,
            ),
            (
                Arc::new(Field::new("name", DataType::Utf8, false)),
                Arc::new(self.name.finish()) as ArrayRef,
            ),
            (
                Arc::new(Field::new("duration", DataType::Int64, true)),
                Arc::new(self.duration.finish()) as ArrayRef,
            ),
        ])
    }
}

impl<'a> Extend<&'a RetentionPolicyRow> for RetentionPolicyRowBuilder {
    fn extend<T: IntoIterator<Item = &'a RetentionPolicyRow>>(&mut self, iter: T) {
        iter.into_iter().for_each(|row| self.append(row));
    }
}

fn retention_policy_rows_to_batch(rows: &[RetentionPolicyRow]) -> RecordBatch {
    let mut builder = RetentionPolicyRowBuilder::default();
    builder.extend(rows);
    RecordBatch::from(&builder.finish())
}

const AUTOGEN_RETENTION_POLICY: &str = "autogen";

fn split_database_name(db_name: &str) -> (String, String) {
    let mut split = db_name.split('/');
    (
        split.next().unwrap().to_owned(),
        split.next().unwrap_or(AUTOGEN_RETENTION_POLICY).to_owned(),
    )
}

// This implementation is for the Flight service
#[async_trait]
impl QueryDatabase for QueryExecutorImpl {
    async fn namespace(
        &self,
        name: &str,
        span: Option<Span>,
        // We expose the `system` tables by default in the monolithic versions of InfluxDB 3
        _include_debug_info_tables: bool,
    ) -> Result<Option<Arc<dyn QueryNamespace>>, DataFusionError> {
        let _span_recorder = SpanRecorder::new(span);

        let db_schema = self.catalog.db_schema(name).ok_or_else(|| {
            DataFusionError::External(Box::new(QueryExecutorError::DatabaseNotFound {
                db_name: name.into(),
            }))
        })?;
        Ok(Some(Arc::new(Database::new(
            db_schema,
            Arc::clone(&self.write_buffer),
            Arc::clone(&self.exec),
            Arc::clone(&self.datafusion_config),
            Arc::clone(&self.query_log),
            Arc::clone(&self.sys_events_store),
        ))))
    }

    async fn acquire_semaphore(&self, span: Option<Span>) -> InstrumentedAsyncOwnedSemaphorePermit {
        Arc::clone(&self.query_execution_semaphore)
            .acquire_owned(span)
            .await
            .expect("Semaphore should not be closed by anyone")
    }

    fn query_log(&self) -> QueryLogEntries {
        self.query_log.entries()
    }
}

#[derive(Debug, Clone)]
pub struct Database {
    db_schema: Arc<DatabaseSchema>,
    write_buffer: Arc<dyn WriteBuffer>,
    exec: Arc<Executor>,
    datafusion_config: Arc<HashMap<String, String>>,
    query_log: Arc<QueryLog>,
    system_schema_provider: Arc<SystemSchemaProvider>,
}

impl Database {
    pub fn new(
        db_schema: Arc<DatabaseSchema>,
        write_buffer: Arc<dyn WriteBuffer>,
        exec: Arc<Executor>,
        datafusion_config: Arc<HashMap<String, String>>,
        query_log: Arc<QueryLog>,
        sys_events_store: Arc<SysEventStore>,
    ) -> Self {
        let system_schema_provider = Arc::new(SystemSchemaProvider::AllSystemSchemaTables(
            AllSystemSchemaTablesProvider::new(
                Arc::clone(&db_schema),
                Arc::clone(&query_log),
                Arc::clone(&write_buffer),
                Arc::clone(&sys_events_store),
            ),
        ));
        Self {
            db_schema,
            write_buffer,
            exec,
            datafusion_config,
            query_log,
            system_schema_provider,
        }
    }

    fn from_namespace(db: &Self) -> Self {
        Self {
            db_schema: Arc::clone(&db.db_schema),
            write_buffer: Arc::clone(&db.write_buffer),
            exec: Arc::clone(&db.exec),
            datafusion_config: Arc::clone(&db.datafusion_config),
            query_log: Arc::clone(&db.query_log),
            system_schema_provider: Arc::clone(&db.system_schema_provider),
        }
    }

    async fn query_table(&self, table_name: &str) -> Option<Arc<QueryTable>> {
        let table_name: Arc<str> = table_name.into();
        self.db_schema
            .table_schema(Arc::clone(&table_name))
            .map(|schema| {
                Arc::new(QueryTable {
                    db_schema: Arc::clone(&self.db_schema),
                    table_name,
                    schema: schema.clone(),
                    write_buffer: Arc::clone(&self.write_buffer),
                })
            })
    }
}

#[async_trait]
impl QueryNamespace for Database {
    fn retention_time_ns(&self) -> Option<i64> {
        None
    }

    fn record_query(
        &self,
        span_ctx: Option<&SpanContext>,
        query_type: &'static str,
        query_text: QueryText,
        query_params: StatementParams,
    ) -> QueryCompletedToken<StateReceived> {
        let trace_id = span_ctx.map(|ctx| ctx.trace_id);
        let namespace_name: Arc<str> = Arc::from("influxdb3 oss");
        self.query_log.push(
            NamespaceId::new(0),
            namespace_name,
            query_type,
            query_text,
            query_params,
            trace_id,
        )
    }

    fn new_query_context(
        &self,
        span_ctx: Option<SpanContext>,
        _config: Option<&QueryConfig>,
    ) -> IOxSessionContext {
        let mut cfg = self
            .exec
            .new_session_config()
            .with_default_catalog(Arc::new(Self::from_namespace(self)))
            .with_span_context(span_ctx);

        for (k, v) in self.datafusion_config.as_ref() {
            cfg = cfg.with_config_option(k, v);
        }

        let ctx = cfg.build();
        ctx.inner().register_udtf(
            LAST_CACHE_UDTF_NAME,
            Arc::new(LastCacheFunction::new(
                self.db_schema.id,
                self.write_buffer.last_cache_provider(),
            )),
        );
        ctx.inner().register_udtf(
            DISTINCT_CACHE_UDTF_NAME,
            Arc::new(DistinctCacheFunction::new(
                self.db_schema.id,
                self.write_buffer.distinct_cache_provider(),
            )),
        );
        ctx
    }

    fn new_extended_query_context(
        &self,
        _extension: std::option::Option<std::sync::Arc<(dyn iox_query::Extension + 'static)>>,
        _span_ctx: Option<SpanContext>,
        _query_config: Option<&QueryConfig>,
    ) -> IOxSessionContext {
        unimplemented!();
    }
}

impl CatalogProvider for Database {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn schema_names(&self) -> Vec<String> {
        debug!("Database as CatalogProvider::schema_names");
        vec![DEFAULT_SCHEMA.to_string(), SYSTEM_SCHEMA_NAME.to_string()]
    }

    fn schema(&self, name: &str) -> Option<Arc<dyn SchemaProvider>> {
        debug!(schema_name = %name, "Database as CatalogProvider::schema");
        match name {
            DEFAULT_SCHEMA => Some(Arc::new(Self::from_namespace(self))),
            SYSTEM_SCHEMA_NAME => Some(Arc::clone(&self.system_schema_provider) as _),
            _ => None,
        }
    }
}

#[async_trait]
impl SchemaProvider for Database {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn table_names(&self) -> Vec<String> {
        self.db_schema
            .table_names()
            .iter()
            .map(|t| t.to_string())
            .collect()
    }

    async fn table(
        &self,
        table_name: &str,
    ) -> Result<Option<Arc<dyn TableProvider>>, DataFusionError> {
        Ok(self.query_table(table_name).await.map(|qt| qt as _))
    }

    fn table_exist(&self, name: &str) -> bool {
        self.db_schema.table_name_to_id(name).is_some()
    }
}

#[derive(Debug)]
pub struct QueryTable {
    db_schema: Arc<DatabaseSchema>,
    table_name: Arc<str>,
    schema: Schema,
    write_buffer: Arc<dyn WriteBuffer>,
}

impl QueryTable {
    fn chunks(
        &self,
        ctx: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Vec<Arc<dyn QueryChunk>>, DataFusionError> {
        self.write_buffer.get_table_chunks(
            &self.db_schema.name,
            &self.table_name,
            filters,
            projection,
            ctx,
        )
    }
}

#[async_trait]
impl TableProvider for QueryTable {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn schema(&self) -> SchemaRef {
        self.schema.as_arrow()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> datafusion::common::Result<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Inexact; filters.len()])
    }

    async fn scan(
        &self,
        ctx: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        let filters = filters.to_vec();
        debug!(
            ?projection,
            ?filters,
            ?limit,
            "QueryTable as TableProvider::scan"
        );
        let mut builder = ProviderBuilder::new(Arc::clone(&self.table_name), self.schema.clone());

        let chunks = self.chunks(ctx, projection, &filters, limit)?;
        for chunk in chunks {
            builder = builder.add_chunk(chunk);
        }

        let provider = match builder.build() {
            Ok(provider) => provider,
            Err(e) => panic!("unexpected error: {e:?}"),
        };

        provider.scan(ctx, projection, &filters, limit).await
    }
}
#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, sync::Arc, time::Duration};

    use crate::query_executor::QueryExecutorImpl;
    use arrow::array::RecordBatch;
    use data_types::NamespaceName;
    use datafusion::assert_batches_sorted_eq;
    use futures::TryStreamExt;
    use influxdb3_cache::{
        distinct_cache::DistinctCacheProvider, last_cache::LastCacheProvider,
        parquet_cache::test_cached_obj_store_and_oracle,
    };
    use influxdb3_catalog::catalog::Catalog;
    use influxdb3_internal_api::query_executor::{QueryExecutor, QueryKind};
    use influxdb3_sys_events::SysEventStore;
    use influxdb3_telemetry::store::TelemetryStore;
    use influxdb3_wal::{Gen1Duration, WalConfig};
    use influxdb3_write::{
        persister::Persister,
        write_buffer::{persisted_files::PersistedFiles, WriteBufferImpl, WriteBufferImplArgs},
        WriteBuffer,
    };
    use iox_query::exec::{DedicatedExecutor, Executor, ExecutorConfig};
    use iox_time::{MockProvider, Time};
    use metric::Registry;
    use object_store::{local::LocalFileSystem, ObjectStore};
    use parquet_file::storage::{ParquetStorage, StorageId};

    use super::CreateQueryExecutorArgs;

    fn make_exec(object_store: Arc<dyn ObjectStore>) -> Arc<Executor> {
        let metrics = Arc::new(metric::Registry::default());

        let parquet_store = ParquetStorage::new(
            Arc::clone(&object_store),
            StorageId::from("test_exec_storage"),
        );
        Arc::new(Executor::new_with_config_and_executor(
            ExecutorConfig {
                target_query_partitions: NonZeroUsize::new(1).unwrap(),
                object_stores: [&parquet_store]
                    .into_iter()
                    .map(|store| (store.id(), Arc::clone(store.object_store())))
                    .collect(),
                metric_registry: Arc::clone(&metrics),
                // Default to 1gb
                mem_pool_size: 1024 * 1024 * 1024, // 1024 (b/kb) * 1024 (kb/mb) * 1024 (mb/gb)
            },
            DedicatedExecutor::new_testing(),
        ))
    }

    async fn setup() -> (Arc<dyn WriteBuffer>, QueryExecutorImpl, Arc<MockProvider>) {
        // Set up QueryExecutor
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(LocalFileSystem::new_with_prefix(test_helpers::tmp_dir().unwrap()).unwrap());
        let time_provider = Arc::new(MockProvider::new(Time::from_timestamp_nanos(0)));
        let (object_store, parquet_cache) = test_cached_obj_store_and_oracle(
            object_store,
            Arc::clone(&time_provider) as _,
            Default::default(),
        );
        let persister = Arc::new(Persister::new(Arc::clone(&object_store), "test_host"));
        let exec = make_exec(Arc::clone(&object_store));
        let host_id = Arc::from("sample-host-id");
        let instance_id = Arc::from("instance-id");
        let catalog = Arc::new(Catalog::new(host_id, instance_id));
        let write_buffer_impl = WriteBufferImpl::new(WriteBufferImplArgs {
            persister,
            catalog: Arc::clone(&catalog),
            last_cache: LastCacheProvider::new_from_catalog(Arc::clone(&catalog)).unwrap(),
            distinct_cache: DistinctCacheProvider::new_from_catalog(
                Arc::<MockProvider>::clone(&time_provider),
                Arc::clone(&catalog),
            )
            .unwrap(),
            time_provider: Arc::<MockProvider>::clone(&time_provider),
            executor: Arc::clone(&exec),
            wal_config: WalConfig {
                gen1_duration: Gen1Duration::new_1m(),
                max_write_buffer_size: 100,
                flush_interval: Duration::from_millis(10),
                snapshot_size: 1,
            },
            parquet_cache: Some(parquet_cache),
            metric_registry: Default::default(),
        })
        .await
        .unwrap();

        let persisted_files: Arc<PersistedFiles> = Arc::clone(&write_buffer_impl.persisted_files());
        let telemetry_store = TelemetryStore::new_without_background_runners(persisted_files);
        let sys_events_store = Arc::new(SysEventStore::new(Arc::<MockProvider>::clone(
            &time_provider,
        )));
        let write_buffer: Arc<dyn WriteBuffer> = write_buffer_impl;
        let metrics = Arc::new(Registry::new());
        let datafusion_config = Arc::new(Default::default());
        let query_executor = QueryExecutorImpl::new(CreateQueryExecutorArgs {
            catalog: write_buffer.catalog(),
            write_buffer: Arc::clone(&write_buffer),
            exec,
            metrics,
            datafusion_config,
            query_log_size: 10,
            telemetry_store,
            sys_events_store,
        });

        (write_buffer, query_executor, time_provider)
    }

    #[test_log::test(tokio::test)]
    async fn system_parquet_files_success() {
        let (write_buffer, query_executor, time_provider) = setup().await;
        // Perform some writes to multiple tables
        let db_name = "test_db";
        // perform writes over time to generate WAL files and some snapshots
        // the time provider is bumped to trick the system into persisting files:
        for i in 0..10 {
            let time = i * 10;
            let _ = write_buffer
                .write_lp(
                    NamespaceName::new(db_name).unwrap(),
                    "\
                cpu,host=a,region=us-east usage=250\n\
                mem,host=a,region=us-east usage=150000\n\
                ",
                    Time::from_timestamp_nanos(time),
                    false,
                    influxdb3_write::Precision::Nanosecond,
                )
                .await
                .unwrap();

            time_provider.set(Time::from_timestamp(time + 1, 0).unwrap());
        }

        // bump time again and sleep briefly to ensure time to persist things
        time_provider.set(Time::from_timestamp(20, 0).unwrap());
        tokio::time::sleep(Duration::from_millis(500)).await;

        struct TestCase<'a> {
            query: &'a str,
            expected: &'a [&'a str],
        }

        let test_cases = [
            TestCase {
                query: "\
                    SELECT table_name, size_bytes, row_count, min_time, max_time \
                    FROM system.parquet_files \
                    WHERE table_name = 'cpu'",
                expected: &[
                    "+------------+------------+-----------+----------+----------+",
                    "| table_name | size_bytes | row_count | min_time | max_time |",
                    "+------------+------------+-----------+----------+----------+",
                    "| cpu        | 1961       | 3         | 0        | 20       |",
                    "| cpu        | 1961       | 3         | 30       | 50       |",
                    "| cpu        | 1961       | 3         | 60       | 80       |",
                    "+------------+------------+-----------+----------+----------+",
                ],
            },
            TestCase {
                query: "\
                    SELECT table_name, size_bytes, row_count, min_time, max_time \
                    FROM system.parquet_files \
                    WHERE table_name = 'mem'",
                expected: &[
                    "+------------+------------+-----------+----------+----------+",
                    "| table_name | size_bytes | row_count | min_time | max_time |",
                    "+------------+------------+-----------+----------+----------+",
                    "| mem        | 1961       | 3         | 0        | 20       |",
                    "| mem        | 1961       | 3         | 30       | 50       |",
                    "| mem        | 1961       | 3         | 60       | 80       |",
                    "+------------+------------+-----------+----------+----------+",
                ],
            },
            TestCase {
                query: "\
                    SELECT table_name, size_bytes, row_count, min_time, max_time \
                    FROM system.parquet_files",
                expected: &[
                    "+------------+------------+-----------+----------+----------+",
                    "| table_name | size_bytes | row_count | min_time | max_time |",
                    "+------------+------------+-----------+----------+----------+",
                    "| cpu        | 1961       | 3         | 0        | 20       |",
                    "| cpu        | 1961       | 3         | 30       | 50       |",
                    "| cpu        | 1961       | 3         | 60       | 80       |",
                    "| mem        | 1961       | 3         | 0        | 20       |",
                    "| mem        | 1961       | 3         | 30       | 50       |",
                    "| mem        | 1961       | 3         | 60       | 80       |",
                    "+------------+------------+-----------+----------+----------+",
                ],
            },
            TestCase {
                query: "\
                    SELECT table_name, size_bytes, row_count, min_time, max_time \
                    FROM system.parquet_files \
                    LIMIT 4",
                expected: &[
                    "+------------+------------+-----------+----------+----------+",
                    "| table_name | size_bytes | row_count | min_time | max_time |",
                    "+------------+------------+-----------+----------+----------+",
                    "| cpu        | 1961       | 3         | 0        | 20       |",
                    "| cpu        | 1961       | 3         | 30       | 50       |",
                    "| cpu        | 1961       | 3         | 60       | 80       |",
                    "| mem        | 1961       | 3         | 60       | 80       |",
                    "+------------+------------+-----------+----------+----------+",
                ],
            },
        ];

        for t in test_cases {
            let batch_stream = query_executor
                .query(db_name, t.query, None, QueryKind::Sql, None, None)
                .await
                .unwrap();
            let batches: Vec<RecordBatch> = batch_stream.try_collect().await.unwrap();
            assert_batches_sorted_eq!(t.expected, &batches);
        }
    }
}
