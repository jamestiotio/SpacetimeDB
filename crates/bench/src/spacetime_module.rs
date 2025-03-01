use spacetimedb::db::datastore::traits::TableSchema;
use spacetimedb::db::{Config, FsyncPolicy, Storage};
use spacetimedb_lib::sats::product;
use spacetimedb_lib::{sats::ArrayValue, AlgebraicValue, ProductValue};
use spacetimedb_testing::modules::{start_runtime, CompilationMode, CompiledModule, ModuleHandle};
use tokio::runtime::Runtime;

use crate::{
    create_schema,
    database::BenchDatabase,
    schemas::{snake_case_table_name, table_name, BenchTable},
    ResultBench,
};

lazy_static::lazy_static! {
    pub static ref BENCHMARKS_MODULE: CompiledModule =
        CompiledModule::compile("benchmarks", CompilationMode::Release);
}

/// A benchmark backend that invokes a spacetime module.
///
/// This is tightly tied to the file `modules/benchmarks/src/lib.rs`;
/// all of the implementations of `BenchDatabase` methods just invoke reducers
/// in that module.
///
/// See the doc comment there for information on the formatting expected for
/// table and reducer names.
pub struct SpacetimeModule {
    runtime: Runtime,
    /// This is here due to Drop shenanigans.
    /// It should always be Some when the module is not being dropped.
    module: Option<ModuleHandle>,
}

impl Drop for SpacetimeModule {
    fn drop(&mut self) {
        // Module must be dropped BEFORE runtime,
        // otherwise there is a deadlock!
        drop(self.module.take());
    }
}

// Note: we use block_on for the methods here. It adds about 70ns of overhead.
// This isn't currently a problem. Overhead to call an empty reducer is currently 20_000 ns.
// It's easier to do it this way because async traits are a mess.
impl BenchDatabase for SpacetimeModule {
    fn name() -> &'static str {
        "stdb_module"
    }

    type TableId = TableId;

    fn build(in_memory: bool, fsync: bool) -> ResultBench<Self>
    where
        Self: Sized,
    {
        let runtime = start_runtime();
        let config = Config {
            fsync: if fsync {
                FsyncPolicy::EveryTx
            } else {
                FsyncPolicy::Never
            },
            storage: if in_memory { Storage::Memory } else { Storage::Disk },
        };
        let module = runtime.block_on(async { BENCHMARKS_MODULE.load_module(config).await });

        for thing in module.client.module.catalog().iter() {
            log::trace!("SPACETIME_MODULE: LOADED: {} {:?}", thing.0, thing.1.ty());
        }
        Ok(SpacetimeModule {
            runtime,
            module: Some(module),
        })
    }

    fn create_table<T: BenchTable>(
        &mut self,
        table_style: crate::schemas::IndexStrategy,
    ) -> ResultBench<Self::TableId> {
        // Noop. All tables are built into the "benchmarks" module.
        Ok(TableId {
            pascal_case: table_name::<T>(table_style),
            snake_case: snake_case_table_name::<T>(table_style),
        })
    }

    fn get_table<T: BenchTable>(&mut self, table_id: &Self::TableId) -> ResultBench<TableSchema> {
        Ok(create_schema::<T>(&table_id.pascal_case))
    }

    fn clear_table(&mut self, table_id: &Self::TableId) -> ResultBench<()> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();
        runtime.block_on(async move {
            // FIXME: this doesn't work. delete is unimplemented!!
            /*
            let name = format!("clear_table_{}", table_id.snake_case);
            module.call_reducer_binary(&name, ProductValue::new(&[])).await?;
            */
            // workaround for now
            module.client.module.clear_table(table_id.pascal_case.clone()).await?;
            Ok(())
        })
    }

    // Implemented by calling a reducer that logs, then looking for the resulting
    // message in the log.
    // This implementation will not work if other people are concurrently interacting with our module.
    fn count_table(&mut self, table_id: &Self::TableId) -> ResultBench<u32> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();

        let count = runtime.block_on(async move {
            let name = format!("count_{}", table_id.snake_case);
            module.call_reducer_binary(&name, ProductValue::new(&[])).await?;
            let logs = module.read_log(Some(1)).await;
            let message = serde_json::from_str::<LoggerRecord>(&logs)?;
            if !message.message.starts_with("COUNT: ") {
                anyhow::bail!("Improper count message format: {:?}", message.message);
            }

            let count = message.message["COUNT: ".len()..].parse::<u32>()?;
            Ok(count)
        })?;
        Ok(count)
    }

    fn empty_transaction(&mut self) -> ResultBench<()> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();

        runtime.block_on(async move {
            module.call_reducer_binary("empty", ProductValue::new(&[])).await?;
            Ok(())
        })
    }

    fn insert<T: BenchTable>(&mut self, table_id: &Self::TableId, row: T) -> ResultBench<()> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();
        let reducer_name = format!("insert_{}", table_id.snake_case);

        runtime.block_on(async move {
            module
                .call_reducer_binary(&reducer_name, row.into_product_value())
                .await?;
            Ok(())
        })
    }

    fn insert_bulk<T: BenchTable>(&mut self, table_id: &Self::TableId, rows: Vec<T>) -> ResultBench<()> {
        let rows = rows.into_iter().map(|row| row.into_product_value()).collect();
        let args = product![ArrayValue::Product(rows)];
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();
        let reducer_name = format!("insert_bulk_{}", table_id.snake_case);

        runtime.block_on(async move {
            module.call_reducer_binary(&reducer_name, args).await?;
            Ok(())
        })
    }

    fn iterate(&mut self, table_id: &Self::TableId) -> ResultBench<()> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();
        let reducer_name = format!("iterate_{}", table_id.snake_case);

        runtime.block_on(async move {
            module
                .call_reducer_binary(&reducer_name, ProductValue::new(&[]))
                .await?;
            Ok(())
        })
    }

    fn filter<T: BenchTable>(
        &mut self,
        table: &TableSchema,
        column_index: u32,
        value: AlgebraicValue,
    ) -> ResultBench<()> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();

        //let product_type = T::product_type();
        let column_name = &table.columns[column_index as usize].col_name;
        let reducer_name = format!("filter_{}_by_{}", table.table_name, column_name);

        runtime.block_on(async move {
            module
                .call_reducer_binary(&reducer_name, ProductValue { elements: vec![value] })
                .await?;
            Ok(())
        })
    }

    fn sql_select(&mut self, table: &TableSchema) -> ResultBench<()> {
        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();
        let sql = format!("SELECT * FROM {}", table.table_name);
        let id = module.client.id.identity;
        runtime.block_on(async move {
            module.client.module.one_off_query(id, sql).await?;
            Ok(())
        })
    }

    fn sql_where<T: BenchTable>(
        &mut self,
        table: &TableSchema,
        column_index: u32,
        value: AlgebraicValue,
    ) -> ResultBench<()> {
        let column = &table.columns[column_index as usize].col_name;

        let value = match value {
            AlgebraicValue::U32(x) => x.to_string(),
            AlgebraicValue::U64(x) => x.to_string(),
            AlgebraicValue::String(x) => format!("'{}'", x),
            _ => {
                unreachable!()
            }
        };

        let SpacetimeModule { runtime, module } = self;
        let module = module.as_mut().unwrap();
        let table_name = &table.table_name;
        let sql_query = format!("SELECT * FROM {table_name} WHERE {column} = {value}");

        let id = module.client.id.identity;
        runtime.block_on(async move {
            module.client.module.one_off_query(id, sql_query).await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct TableId {
    pascal_case: String,
    snake_case: String,
}

#[allow(unused)]
/// Used to parse output from module logs.
///
/// Sync with: `core::database_logger::Record`. We can't use it
/// directly because the types are wrong for deserialization.
/// (Rust!)
#[derive(serde::Deserialize)]
struct LoggerRecord {
    target: Option<String>,
    filename: Option<String>,
    line_number: Option<u32>,
    message: String,
}
