// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, bail};
use aws_arn::{Resource, ARN};
use itertools::Itertools;
use lazy_static::lazy_static;
use regex::Regex;
use rusoto_core::Region;
use url::Url;

use dataflow_types::{
    AvroEncoding, AvroOcfEncoding, AvroOcfSinkConnectorBuilder, Consistency, CsvEncoding,
    DataEncoding, Envelope, ExternalSourceConnector, FileSourceConnector,
    KafkaSinkConnectorBuilder, KafkaSourceConnector, KinesisSourceConnector, ProtobufEncoding,
    RegexEncoding, SinkConnectorBuilder, SourceConnector,
};
use expr::{GlobalId, RowSetFinishing};
use interchange::avro::{self, DebeziumDeduplicationStrategy, Encoder};
use ore::collections::CollectionExt;
use ore::iter::IteratorExt;
use repr::{strconv, Datum, RelationDesc, RelationType, Row, ScalarType};
use sql_parser::ast::{
    AlterIndexOptionsList, AlterIndexOptionsStatement, AlterObjectRenameStatement, AvroSchema,
    ColumnOption, Connector, CreateDatabaseStatement, CreateIndexStatement, CreateSchemaStatement,
    CreateSinkStatement, CreateSourceStatement, CreateTableStatement, CreateViewStatement,
    DropDatabaseStatement, DropObjectsStatement, ExplainStage, ExplainStatement, Explainee, Expr,
    Format, Ident, IfExistsBehavior, InsertStatement, ObjectName, ObjectType, Query,
    SelectStatement, SetVariableStatement, SetVariableValue, ShowColumnsStatement,
    ShowCreateIndexStatement, ShowCreateSinkStatement, ShowCreateSourceStatement,
    ShowCreateTableStatement, ShowCreateViewStatement, ShowDatabasesStatement,
    ShowIndexesStatement, ShowObjectsStatement, ShowStatementFilter, ShowVariableStatement,
    SqlOption, Statement, TailStatement, Value,
};

use crate::catalog::{Catalog, CatalogItemType};
use crate::kafka_util;
use crate::names::{DatabaseSpecifier, FullName, PartialName, SchemaSpecifier};
use crate::normalize;
use crate::parse::parse;
use crate::plan::error::PlanError;
use crate::plan::query::QueryLifetime;
use crate::plan::{
    query, scalar_type_from_sql, AlterIndexLogicalCompactionWindow, Index, LogicalCompactionWindow,
    Params, PeekWhen, Plan, PlanContext, Sink, Source, Table, View,
};
use crate::pure::Schema;

lazy_static! {
    static ref SHOW_DATABASES_DESC: RelationDesc =
        RelationDesc::empty().with_column("database", ScalarType::String.nullable(false));
    static ref SHOW_INDEXES_DESC: RelationDesc = RelationDesc::empty()
        .with_column("On_name", ScalarType::String.nullable(false))
        .with_column("Key_name", ScalarType::String.nullable(false))
        .with_column("Column_name", ScalarType::String.nullable(true))
        .with_column("Expression", ScalarType::String.nullable(true))
        .with_column("Null", ScalarType::Bool.nullable(false))
        .with_column("Seq_in_index", ScalarType::Int64.nullable(false));
    static ref SHOW_COLUMNS_DESC: RelationDesc = RelationDesc::empty()
        .with_column("Field", ScalarType::String.nullable(false))
        .with_column("Nullable", ScalarType::String.nullable(false))
        .with_column("Type", ScalarType::String.nullable(false));
}

pub fn make_show_objects_desc(
    object_type: ObjectType,
    materialized: bool,
    full: bool,
) -> RelationDesc {
    let col_name = object_type_as_plural_str(object_type);
    if full {
        let mut relation_desc = RelationDesc::empty()
            .with_column(col_name, ScalarType::String.nullable(false))
            .with_column("TYPE", ScalarType::String.nullable(false));
        if !materialized && (ObjectType::View == object_type || ObjectType::Source == object_type) {
            relation_desc =
                relation_desc.with_column("MATERIALIZED", ScalarType::Bool.nullable(false));
        }
        relation_desc
    } else {
        RelationDesc::empty().with_column(col_name, ScalarType::String.nullable(false))
    }
}

pub fn describe_statement(
    catalog: &dyn Catalog,
    stmt: Statement,
    param_types_in: &[Option<pgrepr::Type>],
) -> Result<(Option<RelationDesc>, Vec<ScalarType>), anyhow::Error> {
    let mut param_types = BTreeMap::new();
    for (i, ty) in param_types_in.iter().enumerate() {
        if let Some(ty) = ty {
            param_types.insert(i + 1, query::scalar_type_from_pg(ty)?);
        }
    }
    let scx = StatementContext {
        catalog,
        pcx: &PlanContext::default(),
        param_types: Rc::new(RefCell::new(param_types)),
    };
    Ok(match stmt {
        Statement::CreateDatabase(_)
        | Statement::CreateSchema(_)
        | Statement::CreateIndex(_)
        | Statement::CreateSource(_)
        | Statement::CreateTable(_)
        | Statement::CreateSink(_)
        | Statement::CreateView(_)
        | Statement::DropDatabase(_)
        | Statement::DropObjects(_)
        | Statement::SetVariable(_)
        | Statement::StartTransaction(_)
        | Statement::Rollback(_)
        | Statement::Commit(_)
        | Statement::AlterObjectRename(_)
        | Statement::AlterIndexOptions(_) => (None, vec![]),

        Statement::Explain(ExplainStatement {
            stage, explainee, ..
        }) => (
            Some(RelationDesc::empty().with_column(
                match stage {
                    ExplainStage::RawPlan => "Raw Plan",
                    ExplainStage::DecorrelatedPlan => "Decorrelated Plan",
                    ExplainStage::OptimizedPlan { .. } => "Optimized Plan",
                },
                ScalarType::String.nullable(false),
            )),
            match explainee {
                Explainee::Query(q) => {
                    describe_statement(
                        catalog,
                        Statement::Select(SelectStatement {
                            query: q,
                            as_of: None,
                        }),
                        param_types_in,
                    )?
                    .1
                }
                _ => vec![],
            },
        ),

        Statement::ShowCreateView(_) => (
            Some(
                RelationDesc::empty()
                    .with_column("View", ScalarType::String.nullable(false))
                    .with_column("Create View", ScalarType::String.nullable(false)),
            ),
            vec![],
        ),

        Statement::ShowCreateSource(_) => (
            Some(
                RelationDesc::empty()
                    .with_column("Source", ScalarType::String.nullable(false))
                    .with_column("Create Source", ScalarType::String.nullable(false)),
            ),
            vec![],
        ),

        Statement::ShowCreateTable(_) => (
            Some(
                RelationDesc::empty()
                    .with_column("Table", ScalarType::String.nullable(false))
                    .with_column("Create Table", ScalarType::String.nullable(false)),
            ),
            vec![],
        ),

        Statement::ShowCreateSink(_) => (
            Some(
                RelationDesc::empty()
                    .with_column("Sink", ScalarType::String.nullable(false))
                    .with_column("Create Sink", ScalarType::String.nullable(false)),
            ),
            vec![],
        ),

        Statement::ShowCreateIndex(_) => (
            Some(
                RelationDesc::empty()
                    .with_column("Index", ScalarType::String.nullable(false))
                    .with_column("Create Index", ScalarType::String.nullable(false)),
            ),
            vec![],
        ),

        Statement::ShowColumns(_) => (Some(SHOW_COLUMNS_DESC.clone()), vec![]),

        Statement::ShowIndexes(_) => (Some(SHOW_INDEXES_DESC.clone()), vec![]),

        Statement::ShowDatabases(_) => (Some(SHOW_DATABASES_DESC.clone()), vec![]),

        Statement::ShowObjects(ShowObjectsStatement {
            object_type,
            full,
            materialized,
            ..
        }) => (
            Some(make_show_objects_desc(object_type, materialized, full)),
            vec![],
        ),

        Statement::ShowVariable(ShowVariableStatement { variable, .. }) => {
            if variable.as_str() == unicase::Ascii::new("ALL") {
                (
                    Some(
                        RelationDesc::empty()
                            .with_column("name", ScalarType::String.nullable(false))
                            .with_column("setting", ScalarType::String.nullable(false))
                            .with_column("description", ScalarType::String.nullable(false)),
                    ),
                    vec![],
                )
            } else {
                (
                    Some(
                        RelationDesc::empty()
                            .with_column(variable.as_str(), ScalarType::String.nullable(false)),
                    ),
                    vec![],
                )
            }
        }

        Statement::Tail(TailStatement { name, .. }) => {
            let name = scx.resolve_item(name)?;
            let sql_object = scx.catalog.get_item(&name);
            (Some(sql_object.desc()?.clone()), vec![])
        }

        // TODO(benesch): currently, describing a `SELECT` or `INSERT` query
        // plans the whole query to determine its shape and parameter types,
        // and then throws away that plan. If we were smarter, we'd stash that
        // plan somewhere so we don't have to recompute it when the query is
        // executed.
        Statement::Select(SelectStatement { query, .. }) => {
            let (_relation_expr, desc, _finishing) =
                query::plan_root_query(&scx, query, QueryLifetime::OneShot)?;
            (Some(desc), scx.finalize_param_types()?)
        }
        Statement::Insert(InsertStatement {
            table_name,
            columns,
            source,
            ..
        }) => {
            query::plan_insert_query(&scx, table_name, columns, source)?;
            (None, scx.finalize_param_types()?)
        }

        Statement::Update(_) => bail!("UPDATE statements are not supported"),
        Statement::Delete(_) => bail!("DELETE statements are not supported"),
        Statement::Copy(_) => bail!("COPY statements are not supported"),
        Statement::SetTransaction(_) => bail!("SET TRANSACTION statements are not supported"),
    })
}

pub fn handle_statement(
    pcx: &PlanContext,
    catalog: &dyn Catalog,
    stmt: Statement,
    params: &Params,
) -> Result<Plan, anyhow::Error> {
    let param_types = params
        .types
        .iter()
        .enumerate()
        .map(|(i, ty)| (i + 1, ty.clone()))
        .collect();
    let scx = &StatementContext {
        pcx,
        catalog,
        param_types: Rc::new(RefCell::new(param_types)),
    };
    match stmt {
        Statement::CreateDatabase(stmt) => handle_create_database(scx, stmt),
        Statement::CreateIndex(stmt) => handle_create_index(scx, stmt),
        Statement::CreateSchema(stmt) => handle_create_schema(scx, stmt),
        Statement::CreateSink(stmt) => handle_create_sink(scx, stmt),
        Statement::CreateSource(stmt) => handle_create_source(scx, stmt),
        Statement::CreateTable(stmt) => handle_create_table(scx, stmt),
        Statement::CreateView(stmt) => handle_create_view(scx, stmt, params),
        Statement::DropDatabase(stmt) => handle_drop_database(scx, stmt),
        Statement::DropObjects(stmt) => handle_drop_objects(scx, stmt),
        Statement::AlterObjectRename(stmt) => handle_alter_object_rename(scx, stmt),
        Statement::AlterIndexOptions(stmt) => handle_alter_index_options(scx, stmt),
        Statement::ShowColumns(stmt) => handle_show_columns(scx, stmt),
        Statement::ShowCreateIndex(stmt) => handle_show_create_index(scx, stmt),
        Statement::ShowCreateSink(stmt) => handle_show_create_sink(scx, stmt),
        Statement::ShowCreateSource(stmt) => handle_show_create_source(scx, stmt),
        Statement::ShowCreateTable(stmt) => handle_show_create_table(scx, stmt),
        Statement::ShowCreateView(stmt) => handle_show_create_view(scx, stmt),
        Statement::ShowDatabases(stmt) => handle_show_databases(scx, stmt),
        Statement::ShowIndexes(stmt) => handle_show_indexes(scx, stmt),
        Statement::ShowObjects(stmt) => handle_show_objects(scx, stmt),
        Statement::SetVariable(stmt) => handle_set_variable(scx, stmt),
        Statement::ShowVariable(stmt) => handle_show_variable(scx, stmt),

        Statement::Explain(stmt) => handle_explain(scx, stmt, params),
        Statement::Select(stmt) => handle_select(scx, stmt, params),
        Statement::Tail(stmt) => handle_tail(scx, stmt),

        Statement::Insert(stmt) => handle_insert(scx, stmt, params),

        Statement::StartTransaction(_) => Ok(Plan::StartTransaction),
        Statement::Rollback(_) => Ok(Plan::AbortTransaction),
        Statement::Commit(_) => Ok(Plan::CommitTransaction),

        Statement::Update(_) => bail!("UPDATE statements are not supported"),
        Statement::Delete(_) => bail!("DELETE statements are not supported"),
        Statement::Copy(_) => bail!("COPY statements are not supported"),
        Statement::SetTransaction(_) => bail!("SET TRANSACTION statements are not supported"),
    }
}

fn handle_set_variable(
    _: &StatementContext,
    SetVariableStatement {
        local,
        variable,
        value,
    }: SetVariableStatement,
) -> Result<Plan, anyhow::Error> {
    if local {
        unsupported!("SET LOCAL");
    }
    Ok(Plan::SetVariable {
        name: variable.to_string(),
        value: match value {
            SetVariableValue::Literal(Value::String(s)) => s,
            SetVariableValue::Literal(lit) => lit.to_string(),
            SetVariableValue::Ident(ident) => ident.value(),
        },
    })
}

fn handle_show_variable(
    _: &StatementContext,
    ShowVariableStatement { variable }: ShowVariableStatement,
) -> Result<Plan, anyhow::Error> {
    if variable.as_str() == unicase::Ascii::new("ALL") {
        Ok(Plan::ShowAllVariables)
    } else {
        Ok(Plan::ShowVariable(variable.to_string()))
    }
}

fn handle_tail(
    scx: &StatementContext,
    TailStatement {
        name,
        as_of,
        with_snapshot,
    }: TailStatement,
) -> Result<Plan, anyhow::Error> {
    let from = scx.resolve_item(name)?;
    let entry = scx.catalog.get_item(&from);
    let ts = as_of.map(|e| query::eval_as_of(scx, e)).transpose()?;

    match entry.item_type() {
        CatalogItemType::Table | CatalogItemType::Source | CatalogItemType::View => {
            Ok(Plan::Tail {
                id: entry.id(),
                ts,
                with_snapshot,
            })
        }
        CatalogItemType::Index | CatalogItemType::Sink => bail!(
            "'{}' cannot be tailed because it is a {}",
            from,
            entry.item_type(),
        ),
    }
}

fn handle_alter_object_rename(
    scx: &StatementContext,
    AlterObjectRenameStatement {
        name,
        object_type,
        if_exists,
        to_item_name,
    }: AlterObjectRenameStatement,
) -> Result<Plan, anyhow::Error> {
    let id = match scx.resolve_item(name.clone()) {
        Ok(from_name) => {
            let entry = scx.catalog.get_item(&from_name);
            if entry.item_type() != object_type {
                bail!("{} is a {} not a {}", name, entry.item_type(), object_type)
            }
            let mut proposed_name = name.0;
            let last = proposed_name.last_mut().unwrap();
            *last = to_item_name.clone();
            if scx.resolve_item(ObjectName(proposed_name)).is_ok() {
                bail!("{} is already taken by item in schema", to_item_name)
            }
            Some(entry.id())
        }
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating this
            // item does not exist.
            None
        }
        Err(err) => return Err(err.into()),
    };

    Ok(Plan::AlterItemRename {
        id,
        to_name: normalize::ident(to_item_name),
        object_type,
    })
}

fn handle_alter_index_options(
    scx: &StatementContext,
    AlterIndexOptionsStatement {
        index_name,
        if_exists,
        options,
    }: AlterIndexOptionsStatement,
) -> Result<Plan, anyhow::Error> {
    let alter_index = match scx.resolve_item(index_name) {
        Ok(name) => {
            let entry = scx.catalog.get_item(&name);
            if entry.item_type() != CatalogItemType::Index {
                bail!("{} is a {} not a index", name, entry.item_type())
            }

            let logical_compaction_window = match options {
                AlterIndexOptionsList::Reset(o) => {
                    let mut options: HashSet<_> =
                        o.iter().map(|x| normalize::ident(x.clone())).collect();
                    // Follow Postgres and don't complain if unknown parameters
                    // are passed into ALTER INDEX ... RESET
                    if options.remove("logical_compaction_window") {
                        Some(LogicalCompactionWindow::Default)
                    } else {
                        None
                    }
                }
                AlterIndexOptionsList::Set(o) => {
                    let mut options = normalize::options(&o);

                    let logical_compaction_window = match options
                        .remove("logical_compaction_window")
                    {
                        Some(Value::String(window)) => match window.as_str() {
                            "off" => Some(LogicalCompactionWindow::Off),
                            s => Some(LogicalCompactionWindow::Custom(parse_duration::parse(s)?)),
                        },
                        Some(_) => bail!("\"logical_compaction_window\" must be a string"),
                        None => None,
                    };

                    if !options.is_empty() {
                        bail!("unrecognized parameter: \"{}\". Only \"logical_compaction_window\" is currently supported.",
                              options.keys().next().expect("known to exist"))
                    }

                    logical_compaction_window
                }
            };

            if let Some(logical_compaction_window) = logical_compaction_window {
                Some(AlterIndexLogicalCompactionWindow {
                    index: entry.id(),
                    logical_compaction_window,
                })
            } else {
                None
            }
        }
        Err(_) if if_exists => {
            // TODO(rkhaitan): better message indicating that the index does not exist.
            None
        }
        Err(e) => return Err(e.into()),
    };

    Ok(Plan::AlterIndexLogicalCompactionWindow(alter_index))
}

fn handle_show_databases(
    scx: &StatementContext,
    ShowDatabasesStatement { filter }: ShowDatabasesStatement,
) -> Result<Plan, anyhow::Error> {
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => {
            format!("AND database LIKE {}", Value::String(like))
        }
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };
    handle_generated_select(
        scx,
        format!(
            "SELECT database
             FROM mz_catalog.mz_databases
             WHERE mz_catalog.mz_databases.id != -1 {}",
            filter
        ),
    )
}

fn handle_show_objects(
    scx: &StatementContext,
    ShowObjectsStatement {
        extended,
        full,
        materialized,
        object_type,
        from,
        filter,
    }: ShowObjectsStatement,
) -> Result<Plan, anyhow::Error> {
    match object_type {
        ObjectType::Schema => handle_show_schemas(scx, extended, full, from, filter),
        ObjectType::Table => handle_show_tables(scx, extended, full, from, filter),
        ObjectType::Source => handle_show_sources(scx, full, materialized, from, filter),
        ObjectType::View => handle_show_views(scx, full, materialized, from, filter),
        ObjectType::Sink => handle_show_sinks(scx, full, from, filter),
        ObjectType::Index => unreachable!("SHOW INDEX handled separately"),
    }
}

fn handle_show_schemas(
    scx: &StatementContext,
    extended: bool,
    full: bool,
    from: Option<ObjectName>,
    filter: Option<ShowStatementFilter>,
) -> Result<Plan, anyhow::Error> {
    let database_name = if let Some(from) = from {
        scx.resolve_database(from)?
    } else {
        scx.resolve_default_database()?.to_string()
    };
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => format!("AND schema LIKE {}", Value::String(like)),
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };

    let query = if !full & !extended {
        format!(
            "SELECT schema
            FROM mz_catalog.mz_schemas
            JOIN mz_catalog.mz_databases ON mz_catalog.mz_schemas.database_id = mz_catalog.mz_databases.id
            WHERE mz_catalog.mz_databases.database = '{}' {}",
            database_name, filter
        )
    } else if full & !extended {
        format!(
            "SELECT schema, type
            FROM mz_catalog.mz_schemas
            JOIN mz_catalog.mz_databases ON mz_catalog.mz_schemas.database_id = mz_catalog.mz_databases.id
            WHERE mz_catalog.mz_databases.database = '{}' {}",
            database_name, filter
        )
    } else if !full & extended {
        // -1 is the ambient database id.
        format!(
            "SELECT schema
            FROM mz_catalog.mz_schemas
            JOIN mz_catalog.mz_databases ON mz_catalog.mz_schemas.database_id = mz_catalog.mz_databases.id
            WHERE mz_catalog.mz_databases.database = '{}' OR mz_catalog.mz_databases.id = -1 {}",
            database_name, filter
        )
    } else {
        // -1 is the ambient database id.
        format!(
            "SELECT schema, type
            FROM mz_catalog.mz_schemas
            JOIN mz_catalog.mz_databases ON mz_catalog.mz_schemas.database_id = mz_catalog.mz_databases.id
            WHERE mz_catalog.mz_databases.database = '{}' OR mz_catalog.mz_databases.id = -1 {}",
            database_name, filter
        )
    };
    handle_generated_select(scx, query)
}

fn handle_show_sinks(
    scx: &StatementContext,
    full: bool,
    from: Option<ObjectName>,
    filter: Option<ShowStatementFilter>,
) -> Result<Plan, anyhow::Error> {
    let schema_spec = if let Some(from) = from {
        scx.resolve_schema(from)?.1
    } else {
        scx.resolve_default_schema()?
    };
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => format!("AND sinks LIKE {}", Value::String(like)),
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };

    let query = if full {
        format!(
            "SELECT sinks, type
            FROM mz_catalog.mz_sinks
            JOIN mz_catalog.mz_schemas ON mz_catalog.mz_sinks.schema_id = mz_catalog.mz_schemas.schema_id
            WHERE schema_id = {} {}
            ORDER BY sinks, type",
            schema_spec.id, filter
        )
    } else {
        format!(
            "SELECT sinks FROM mz_catalog.mz_sinks WHERE schema_id = {} {} ORDER BY sinks",
            schema_spec.id, filter
        )
    };
    handle_generated_select(scx, query)
}

fn handle_show_views(
    scx: &StatementContext,
    full: bool,
    materialized: bool,
    from: Option<ObjectName>,
    filter: Option<ShowStatementFilter>,
) -> Result<Plan, anyhow::Error> {
    let schema_spec = if let Some(from) = from {
        scx.resolve_schema(from)?.1
    } else {
        scx.resolve_default_schema()?
    };
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => format!("AND views LIKE {}", Value::String(like)),
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };

    let query = if !full & !materialized {
        format!(
            "SELECT views
             FROM mz_catalog.mz_views
             WHERE mz_catalog.mz_views.schema_id = {} {}
             ORDER BY views ASC",
            schema_spec.id, filter
        )
    } else if full & !materialized {
        format!(
            "SELECT
                views,
                type,
                count > 0 as materialized
             FROM mz_catalog.mz_views as mz_views
             JOIN mz_catalog.mz_schemas ON mz_catalog.mz_views.schema_id = mz_catalog.mz_schemas.schema_id
             JOIN (SELECT mz_views.global_id as global_id, count(mz_indexes.on_global_id) AS count
                   FROM mz_views
                   LEFT JOIN mz_indexes on mz_views.global_id = mz_indexes.on_global_id
                   GROUP BY mz_views.global_id) as mz_indexes_count
                ON mz_views.global_id = mz_indexes_count.global_id
             WHERE mz_catalog.mz_views.schema_id = {} {}
             ORDER BY views ASC",
            schema_spec.id, filter
        )
    } else if !full & materialized {
        format!(
            "SELECT views
             FROM mz_catalog.mz_views
             JOIN (SELECT mz_views.global_id as global_id, count(mz_indexes.on_global_id) AS count
                   FROM mz_views
                   LEFT JOIN mz_indexes on mz_views.global_id = mz_indexes.on_global_id
                   GROUP BY mz_views.global_id) as mz_indexes_count
                ON mz_views.global_id = mz_indexes_count.global_id
             WHERE mz_catalog.mz_views.schema_id = {}
                AND mz_indexes_count.count > 0 {}
             ORDER BY views ASC",
            schema_spec.id, filter
        )
    } else {
        format!(
            "SELECT views, type
             FROM mz_catalog.mz_views
             JOIN mz_catalog.mz_schemas ON mz_catalog.mz_views.schema_id = mz_catalog.mz_schemas.schema_id
             JOIN (SELECT mz_views.global_id as global_id, count(mz_indexes.on_global_id) AS count
                   FROM mz_views
                   LEFT JOIN mz_indexes on mz_views.global_id = mz_indexes.on_global_id
                   GROUP BY mz_views.global_id) as mz_indexes_count
                ON mz_views.global_id = mz_indexes_count.global_id
             WHERE mz_catalog.mz_views.schema_id = {}
                AND mz_indexes_count.count > 0 {}
             ORDER BY views ASC",
            schema_spec.id, filter
        )
    };
    handle_generated_select(scx, query)
}

fn handle_show_sources(
    scx: &StatementContext,
    full: bool,
    materialized: bool,
    from: Option<ObjectName>,
    filter: Option<ShowStatementFilter>,
) -> Result<Plan, anyhow::Error> {
    let schema_spec = if let Some(from) = from {
        scx.resolve_schema(from)?.1
    } else {
        scx.resolve_default_schema()?
    };
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => {
            format!("AND sources LIKE {}", Value::String(like))
        }
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };

    let query = if !full & !materialized {
        format!(
            "SELECT sources FROM mz_catalog.mz_sources WHERE schema_id = {} {} ORDER BY sources",
            schema_spec.id, filter
        )
    } else if full & !materialized {
        format!(
            "SELECT sources, type, CASE WHEN count > 0 then true ELSE false END materialized
            FROM mz_catalog.mz_sources
            JOIN mz_catalog.mz_schemas ON mz_catalog.mz_sources.schema_id = mz_catalog.mz_schemas.schema_id
            JOIN (SELECT mz_catalog.mz_sources.global_id as global_id, count(mz_catalog.mz_indexes.on_global_id) AS count
                  FROM mz_catalog.mz_sources
                  LEFT JOIN mz_catalog.mz_indexes on mz_catalog.mz_sources.global_id = mz_catalog.mz_indexes.on_global_id
                  GROUP BY mz_catalog.mz_sources.global_id) as mz_indexes_count
                ON mz_catalog.mz_sources.global_id = mz_indexes_count.global_id
            WHERE schema_id = {} {}
            ORDER BY sources, type",
            schema_spec.id, filter
        )
    } else if !full & materialized {
        format!(
            "SELECT sources
            FROM mz_catalog.mz_sources
            JOIN mz_catalog.mz_schemas ON mz_catalog.mz_sources.schema_id = mz_catalog.mz_schemas.schema_id
            JOIN (SELECT mz_catalog.mz_sources.global_id as global_id, count(mz_catalog.mz_indexes.on_global_id) AS count
                  FROM mz_catalog.mz_sources
                  LEFT JOIN mz_catalog.mz_indexes on mz_catalog.mz_sources.global_id = mz_catalog.mz_indexes.on_global_id
                  GROUP BY mz_catalog.mz_sources.global_id) as mz_indexes_count
                ON mz_catalog.mz_sources.global_id = mz_indexes_count.global_id
            WHERE schema_id = {} {} AND mz_indexes_count.count > 0
            ORDER BY sources, type",
            schema_spec.id, filter
        )
    } else {
        format!(
            "SELECT sources, type
            FROM mz_catalog.mz_sources
            JOIN mz_catalog.mz_schemas ON mz_catalog.mz_sources.schema_id = mz_catalog.mz_schemas.schema_id
            JOIN (SELECT mz_catalog.mz_sources.global_id as global_id, count(mz_catalog.mz_indexes.on_global_id) AS count
                  FROM mz_catalog.mz_sources
                  LEFT JOIN mz_catalog.mz_indexes on mz_catalog.mz_sources.global_id = mz_catalog.mz_indexes.on_global_id
                  GROUP BY mz_catalog.mz_sources.global_id) as mz_indexes_count
                ON mz_catalog.mz_sources.global_id = mz_indexes_count.global_id
            WHERE schema_id = {} {} AND mz_indexes_count.count > 0
            ORDER BY sources, type",
            schema_spec.id, filter
        )
    };
    handle_generated_select(scx, query)
}

fn handle_show_tables(
    scx: &StatementContext,
    extended: bool,
    full: bool,
    from: Option<ObjectName>,
    filter: Option<ShowStatementFilter>,
) -> Result<Plan, anyhow::Error> {
    if extended {
        unsupported!("SHOW EXTENDED TABLES");
    }

    let schema_spec = if let Some(from) = from {
        scx.resolve_schema(from)?.1
    } else {
        scx.resolve_default_schema()?
    };
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => format!("AND tables LIKE {}", Value::String(like)),
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };

    let query = if full {
        format!(
            "SELECT tables, type
            FROM mz_catalog.mz_tables
            JOIN mz_catalog.mz_schemas ON mz_catalog.mz_tables.schema_id = mz_catalog.mz_schemas.schema_id
            WHERE schema_id = {} {}
            ORDER BY tables, type",
            schema_spec.id, filter
        )
    } else {
        format!(
            "SELECT tables FROM mz_catalog.mz_tables WHERE schema_id = {} {} ORDER BY tables",
            schema_spec.id, filter
        )
    };
    handle_generated_select(scx, query)
}

fn handle_show_indexes(
    scx: &StatementContext,
    ShowIndexesStatement {
        extended,
        table_name,
        filter,
    }: ShowIndexesStatement,
) -> Result<Plan, anyhow::Error> {
    if extended {
        unsupported!("SHOW EXTENDED INDEXES")
    }
    let from_name = scx.resolve_item(table_name)?;
    let from_entry = scx.catalog.get_item(&from_name);
    if from_entry.item_type() != CatalogItemType::View
        && from_entry.item_type() != CatalogItemType::Source
        && from_entry.item_type() != CatalogItemType::Table
    {
        bail!(
            "cannot show indexes on {} because it is a {}",
            from_name,
            from_entry.item_type(),
        );
    }

    let base_query = format!(
        "SELECT
            on_names.name as on_name,
            index_names.name as key_name,
            mz_catalog.mz_columns.field as column_name,
            mz_catalog.mz_indexes.expression as expression,
            mz_catalog.mz_indexes.nullable as nullable,
            mz_catalog.mz_indexes.seq_in_index as seq_in_index
        FROM
            mz_catalog.mz_indexes
            JOIN mz_catalog.mz_catalog_names AS on_names ON mz_catalog.mz_indexes.on_global_id = on_names.global_id
            JOIN mz_catalog.mz_catalog_names AS index_names ON mz_catalog.mz_indexes.global_id = index_names.global_id
            LEFT OUTER JOIN mz_catalog.mz_columns ON mz_catalog.mz_indexes.on_global_id = mz_catalog.mz_columns.global_id
                AND mz_catalog.mz_indexes.field_number = mz_catalog.mz_columns.field_number
        WHERE
            on_names.name = '{}'
        ORDER BY
            key_name asc,
            seq_in_index asc", from_name
    );

    let query = if let Some(filter) = filter {
        let filter = match filter {
            ShowStatementFilter::Like(like) => format!("key_name LIKE {}", Value::String(like)),
            ShowStatementFilter::Where(expr) => expr.to_string(),
        };
        format!(
            "SELECT on_name, key_name, column_name, expression, nullable, seq_in_index
             FROM ({})
             WHERE {}",
            base_query, filter,
        )
    } else {
        base_query
    };
    handle_generated_select(scx, query)
}

/// Create an immediate result that describes all the columns for the given table
fn handle_show_columns(
    scx: &StatementContext,
    ShowColumnsStatement {
        extended,
        full,
        table_name,
        filter,
    }: ShowColumnsStatement,
) -> Result<Plan, anyhow::Error> {
    if extended {
        unsupported!("SHOW EXTENDED COLUMNS");
    }
    if full {
        unsupported!("SHOW FULL COLUMNS");
    }

    let name = scx.resolve_item(table_name)?;
    let filter = match filter {
        Some(ShowStatementFilter::Like(like)) => format!("AND field LIKE {}", Value::String(like)),
        Some(ShowStatementFilter::Where(expr)) => format!("AND {}", expr.to_string()),
        None => "".to_owned(),
    };
    let query = format!(
        "SELECT
            mz_columns.field,
            CASE WHEN mz_columns.nullable THEN 'YES' ELSE 'NO' END nullable,
            mz_columns.type
         FROM mz_catalog.mz_columns AS mz_columns
         JOIN mz_catalog.mz_catalog_names AS mz_catalog_names ON mz_columns.global_id = mz_catalog_names.global_id
         WHERE mz_catalog_names.name = '{}' {}
         ORDER BY mz_columns.field_number ASC",
        name, filter
    );
    handle_generated_select(scx, query)
}

fn handle_show_create_view(
    scx: &StatementContext,
    ShowCreateViewStatement { view_name }: ShowCreateViewStatement,
) -> Result<Plan, anyhow::Error> {
    let name = scx.resolve_item(view_name)?;
    let entry = scx.catalog.get_item(&name);
    if let CatalogItemType::View = entry.item_type() {
        Ok(Plan::SendRows(vec![Row::pack(&[
            Datum::String(&name.to_string()),
            Datum::String(entry.create_sql()),
        ])]))
    } else {
        bail!("{} is not a view", name);
    }
}

fn handle_show_create_source(
    scx: &StatementContext,
    ShowCreateSourceStatement { source_name }: ShowCreateSourceStatement,
) -> Result<Plan, anyhow::Error> {
    let name = scx.resolve_item(source_name)?;
    let entry = scx.catalog.get_item(&name);
    if let CatalogItemType::Source = entry.item_type() {
        Ok(Plan::SendRows(vec![Row::pack(&[
            Datum::String(&name.to_string()),
            Datum::String(entry.create_sql()),
        ])]))
    } else {
        bail!("{} is not a source", name);
    }
}

fn handle_show_create_table(
    scx: &StatementContext,
    ShowCreateTableStatement { table_name }: ShowCreateTableStatement,
) -> Result<Plan, anyhow::Error> {
    let name = scx.resolve_item(table_name)?;
    let entry = scx.catalog.get_item(&name);
    if let CatalogItemType::Table = entry.item_type() {
        Ok(Plan::SendRows(vec![Row::pack(&[
            Datum::String(&name.to_string()),
            Datum::String(entry.create_sql()),
        ])]))
    } else {
        bail!("{} is not a table", name);
    }
}

fn handle_show_create_sink(
    scx: &StatementContext,
    ShowCreateSinkStatement { sink_name }: ShowCreateSinkStatement,
) -> Result<Plan, anyhow::Error> {
    let name = scx.resolve_item(sink_name)?;
    let entry = scx.catalog.get_item(&name);
    if let CatalogItemType::Sink = entry.item_type() {
        Ok(Plan::SendRows(vec![Row::pack(&[
            Datum::String(&name.to_string()),
            Datum::String(entry.create_sql()),
        ])]))
    } else {
        bail!("'{}' is not a sink", name);
    }
}

fn handle_show_create_index(
    scx: &StatementContext,
    ShowCreateIndexStatement { index_name }: ShowCreateIndexStatement,
) -> Result<Plan, anyhow::Error> {
    let name = scx.resolve_item(index_name)?;
    let entry = scx.catalog.get_item(&name);
    if let CatalogItemType::Index = entry.item_type() {
        Ok(Plan::SendRows(vec![Row::pack(&[
            Datum::String(&name.to_string()),
            Datum::String(entry.create_sql()),
        ])]))
    } else {
        bail!("'{}' is not an index", name);
    }
}

fn kafka_sink_builder(
    format: Option<Format>,
    with_options: Vec<SqlOption>,
    broker: String,
    topic_prefix: String,
    desc: RelationDesc,
    topic_suffix: String,
) -> Result<SinkConnectorBuilder, anyhow::Error> {
    let schema_registry_url = match format {
        Some(Format::Avro(AvroSchema::CsrUrl {
            url,
            seed,
            with_options,
        })) => {
            if seed.is_some() {
                bail!("SEED option does not make sense with sinks");
            }
            if !with_options.is_empty() {
                unsupported!("CONFLUENT SCHEMA REGISTRY ... WITH options in CREATE SINK");
            }
            url.parse()?
        }
        _ => unsupported!("non-confluent schema registry avro sinks"),
    };

    let broker_addrs = broker.parse()?;

    let mut with_options = normalize::options(&with_options);
    let include_consistency = match with_options.remove("consistency") {
        Some(Value::Boolean(b)) => b,
        None => false,
        Some(_) => bail!("consistency must be a boolean"),
    };

    let encoder = Encoder::new(desc, include_consistency);
    let value_schema = encoder.writer_schema().canonical_form();

    // Use the user supplied value for replication factor, or default to 1
    let replication_factor = match with_options.remove("replication_factor") {
        None => 1,
        Some(Value::Number(n)) => n.parse::<u32>()?,
        Some(_) => bail!("replication factor for sink topics has to be a positive integer"),
    };

    if replication_factor == 0 {
        bail!("replication factor for sink topics has to be greater than zero");
    }

    let consistency_value_schema = if include_consistency {
        Some(avro::get_debezium_transaction_schema().canonical_form())
    } else {
        None
    };

    Ok(SinkConnectorBuilder::Kafka(KafkaSinkConnectorBuilder {
        broker_addrs,
        schema_registry_url,
        value_schema,
        topic_prefix,
        topic_suffix,
        replication_factor,
        fuel: 10000,
        consistency_value_schema,
    }))
}

fn avro_ocf_sink_builder(
    format: Option<Format>,
    with_options: Vec<SqlOption>,
    path: String,
    file_name_suffix: String,
) -> Result<SinkConnectorBuilder, anyhow::Error> {
    if format.is_some() {
        bail!("avro ocf sinks cannot specify a format");
    }

    if !with_options.is_empty() {
        bail!("avro ocf sinks do not support WITH options");
    }

    let path = PathBuf::from(path);

    if path.is_dir() {
        bail!("avro ocf sink cannot write to a directory");
    }

    Ok(SinkConnectorBuilder::AvroOcf(AvroOcfSinkConnectorBuilder {
        path,
        file_name_suffix,
    }))
}

fn handle_create_sink(
    scx: &StatementContext,
    stmt: CreateSinkStatement,
) -> Result<Plan, anyhow::Error> {
    let create_sql = normalize::create_statement(scx, Statement::CreateSink(stmt.clone()))?;
    let CreateSinkStatement {
        name,
        from,
        connector,
        with_options,
        format,
        with_snapshot,
        as_of,
        if_not_exists,
    } = stmt;
    let name = scx.allocate_name(normalize::object_name(name)?);
    let from = scx.catalog.get_item(&scx.resolve_item(from)?);
    let suffix = format!(
        "{}-{}",
        scx.catalog
            .startup_time()
            .duration_since(UNIX_EPOCH)?
            .as_secs(),
        scx.catalog.nonce()
    );

    let as_of = as_of.map(|e| query::eval_as_of(scx, e)).transpose()?;
    let connector_builder = match connector {
        Connector::File { .. } => unsupported!("file sinks"),
        Connector::Kafka { broker, topic } => kafka_sink_builder(
            format,
            with_options,
            broker,
            topic,
            from.desc()?.clone(),
            suffix,
        )?,
        Connector::Kinesis { .. } => unsupported!("Kinesis sinks"),
        Connector::AvroOcf { path } => avro_ocf_sink_builder(format, with_options, path, suffix)?,
    };

    Ok(Plan::CreateSink {
        name,
        sink: Sink {
            create_sql,
            from: from.id(),
            connector_builder,
        },
        with_snapshot,
        as_of,
        if_not_exists,
    })
}

fn handle_create_index(
    scx: &StatementContext,
    mut stmt: CreateIndexStatement,
) -> Result<Plan, anyhow::Error> {
    let CreateIndexStatement {
        name,
        on_name,
        key_parts,
        if_not_exists,
    } = &mut stmt;
    let on_name = scx.resolve_item(on_name.clone())?;
    let catalog_entry = scx.catalog.get_item(&on_name);

    if CatalogItemType::View != catalog_entry.item_type()
        && CatalogItemType::Source != catalog_entry.item_type()
        && CatalogItemType::Table != catalog_entry.item_type()
    {
        bail!(
            "index cannot be created on {} because it is a {}",
            on_name,
            catalog_entry.item_type()
        )
    }

    let on_desc = catalog_entry.desc()?;

    let filled_key_parts = match key_parts {
        Some(kp) => kp.to_vec(),
        None => {
            // `key_parts` is None if we're creating a "default" index, i.e.
            // creating the index as if the index had been created alongside the
            // view source, e.g. `CREATE MATERIALIZED...`
            catalog_entry
                .desc()?
                .typ()
                .default_key()
                .iter()
                .map(|i| match on_desc.get_unambiguous_name(*i) {
                    Some(n) => Expr::Identifier(vec![Ident::new(n.to_string())]),
                    _ => Expr::Value(Value::Number((i + 1).to_string())),
                })
                .collect()
        }
    };
    let keys = query::plan_index_exprs(scx, on_desc, filled_key_parts.clone())?;

    let index_name = if let Some(name) = name {
        FullName {
            database: on_name.database.clone(),
            schema: on_name.schema.clone(),
            item: normalize::ident(name.clone()),
        }
    } else {
        let mut idx_name_base = on_name.clone();
        if key_parts.is_none() {
            // We're trying to create the "default" index.
            idx_name_base.item += "_primary_idx";
        } else {
            // Use PG schema for automatically naming indexes:
            // `<table>_<_-separated indexed expressions>_idx`
            let index_name_col_suffix = keys
                .iter()
                .map(|k| match k {
                    expr::ScalarExpr::Column(i) => match on_desc.get_unambiguous_name(*i) {
                        Some(col_name) => col_name.to_string(),
                        None => format!("{}", i + 1),
                    },
                    _ => "expr".to_string(),
                })
                .join("_");
            idx_name_base.item += &format!("_{}_idx", index_name_col_suffix);
            idx_name_base.item = normalize::ident(Ident::new(idx_name_base.item))
        }

        let mut index_name = idx_name_base.clone();
        let mut i = 0;

        let mut cat_schema_iter = scx.catalog.list_items(&on_name.database, &on_name.schema);

        // Search for an unused version of the name unless `if_not_exists`.
        while cat_schema_iter.any(|i| *i.name() == index_name) && !*if_not_exists {
            i += 1;
            index_name = idx_name_base.clone();
            index_name.item += &i.to_string();
            cat_schema_iter = scx.catalog.list_items(&on_name.database, &on_name.schema);
        }

        index_name
    };

    // Normalize `stmt`.
    *name = Some(Ident::new(index_name.item.clone()));
    *key_parts = Some(filled_key_parts);
    let if_not_exists = *if_not_exists;
    let create_sql = normalize::create_statement(scx, Statement::CreateIndex(stmt))?;

    Ok(Plan::CreateIndex {
        name: index_name,
        index: Index {
            create_sql,
            on: catalog_entry.id(),
            keys,
        },
        if_not_exists,
    })
}

fn handle_create_database(
    _: &StatementContext,
    CreateDatabaseStatement {
        name,
        if_not_exists,
    }: CreateDatabaseStatement,
) -> Result<Plan, anyhow::Error> {
    Ok(Plan::CreateDatabase {
        name: normalize::ident(name),
        if_not_exists,
    })
}

fn handle_create_schema(
    scx: &StatementContext,
    CreateSchemaStatement {
        mut name,
        if_not_exists,
    }: CreateSchemaStatement,
) -> Result<Plan, anyhow::Error> {
    if name.0.len() > 2 {
        bail!("schema name {} has more than two components", name);
    }
    let schema_name = normalize::ident(
        name.0
            .pop()
            .expect("names always have at least one component"),
    );
    let database_name = match name.0.pop() {
        None => DatabaseSpecifier::Name(scx.catalog.default_database().into()),
        Some(n) => DatabaseSpecifier::Name(normalize::ident(n)),
    };
    Ok(Plan::CreateSchema {
        database_name,
        schema_name,
        if_not_exists,
    })
}

fn handle_create_view(
    scx: &StatementContext,
    mut stmt: CreateViewStatement,
    params: &Params,
) -> Result<Plan, anyhow::Error> {
    let create_sql = normalize::create_statement(scx, Statement::CreateView(stmt.clone()))?;
    let CreateViewStatement {
        name,
        columns,
        query,
        temporary,
        materialized,
        if_exists,
        with_options,
    } = &mut stmt;
    if !with_options.is_empty() {
        unsupported!("WITH options");
    }
    let name = if *temporary {
        scx.allocate_temporary_name(normalize::object_name(name.to_owned())?)
    } else {
        scx.allocate_name(normalize::object_name(name.to_owned())?)
    };
    let replace = if *if_exists == IfExistsBehavior::Replace
        && scx.catalog.resolve_item(&name.clone().into()).is_ok()
    {
        let cascade = false;
        handle_drop_item(scx, ObjectType::View, &name, cascade)?
    } else {
        None
    };
    let (mut relation_expr, mut desc, finishing) =
        query::plan_root_query(scx, query.clone(), QueryLifetime::Static)?;
    relation_expr.bind_parameters(&params)?;
    //TODO: materialize#724 - persist finishing information with the view?
    relation_expr.finish(finishing);
    let relation_expr = relation_expr.decorrelate();
    desc = maybe_rename_columns(format!("view {}", name), desc, columns)?;
    let temporary = *temporary;
    let materialize = *materialized; // Normalize for `raw_sql` below.
    let if_not_exists = *if_exists == IfExistsBehavior::Skip;
    Ok(Plan::CreateView {
        name,
        view: View {
            create_sql,
            expr: relation_expr,
            column_names: desc.iter_names().map(|n| n.cloned()).collect(),
            temporary,
        },
        replace,
        materialize,
        if_not_exists,
    })
}

fn extract_timestamp_frequency_option(
    with_options: &mut HashMap<String, Value>,
) -> Result<Duration, anyhow::Error> {
    match with_options.remove("timestamp_frequency_ms") {
        None => Ok(Duration::from_secs(1)),
        Some(Value::Number(n)) => match n.parse::<u64>() {
            Ok(n) => Ok(Duration::from_millis(n)),
            _ => bail!("timestamp_frequency_ms must be an u64"),
        },
        Some(_) => bail!("timestamp_frequency_ms must be an u64"),
    }
}

fn handle_create_source(
    scx: &StatementContext,
    stmt: CreateSourceStatement,
) -> Result<Plan, anyhow::Error> {
    let CreateSourceStatement {
        name,
        col_names,
        connector,
        with_options,
        format,
        envelope,
        if_not_exists,
        materialized,
    } = &stmt;
    let get_encoding = |format: &Option<Format>| {
        let format = format
            .as_ref()
            .ok_or_else(|| anyhow!("Source format must be specified"))?;

        Ok(match format {
            Format::Bytes => DataEncoding::Bytes,
            Format::Avro(schema) => {
                let Schema {
                    key_schema,
                    value_schema,
                    schema_registry_config,
                } = match schema {
                    // TODO(jldlaughlin): we need a way to pass in primary key information
                    // when building a source from a string or file.
                    AvroSchema::Schema(sql_parser::ast::Schema::Inline(schema)) => Schema {
                        key_schema: None,
                        value_schema: schema.clone(),
                        schema_registry_config: None,
                    },
                    AvroSchema::Schema(sql_parser::ast::Schema::File(_)) => {
                        unreachable!("File schema should already have been inlined")
                    }
                    AvroSchema::CsrUrl {
                        url,
                        seed,
                        with_options: ccsr_options,
                    } => {
                        let url: Url = url.parse()?;
                        let kafka_options =
                            kafka_util::extract_config(&normalize::options(with_options))?;
                        let ccsr_config = kafka_util::generate_ccsr_client_config(
                            url,
                            &kafka_options,
                            &normalize::options(ccsr_options),
                        )?;

                        if let Some(seed) = seed {
                            Schema {
                                key_schema: seed.key_schema.clone(),
                                value_schema: seed.value_schema.clone(),
                                schema_registry_config: Some(ccsr_config),
                            }
                        } else {
                            unreachable!("CSR seed resolution should already have been called")
                        }
                    }
                };

                DataEncoding::Avro(AvroEncoding {
                    key_schema,
                    value_schema,
                    schema_registry_config,
                })
            }
            Format::Protobuf {
                message_name,
                schema,
            } => {
                let descriptors = match schema {
                    sql_parser::ast::Schema::Inline(bytes) => strconv::parse_bytes(&bytes)?,
                    sql_parser::ast::Schema::File(_) => {
                        unreachable!("File schema should already have been inlined")
                    }
                };

                DataEncoding::Protobuf(ProtobufEncoding {
                    descriptors,
                    message_name: message_name.to_owned(),
                })
            }
            Format::Regex(regex) => {
                let regex = Regex::new(regex)?;
                DataEncoding::Regex(RegexEncoding { regex })
            }
            Format::Csv {
                header_row,
                n_cols,
                delimiter,
            } => {
                let n_cols = if col_names.is_empty() {
                    match n_cols {
                        Some(n) => *n,
                        None => bail!(
                            "Cannot determine number of columns in CSV source; specify using \
                            CREATE SOURCE...FORMAT CSV WITH X COLUMNS"
                        ),
                    }
                } else {
                    col_names.len()
                };
                DataEncoding::Csv(CsvEncoding {
                    header_row: *header_row,
                    n_cols,
                    delimiter: match *delimiter as u32 {
                        0..=127 => *delimiter as u8,
                        _ => bail!("CSV delimiter must be an ASCII character"),
                    },
                })
            }
            Format::Json => unsupported!("JSON sources"),
            Format::Text => DataEncoding::Text,
        })
    };

    let mut with_options = normalize::options(with_options);

    let mut consistency = Consistency::RealTime;
    let mut ts_frequency = Duration::from_secs(1);

    let (external_connector, mut encoding) = match connector {
        Connector::Kafka { broker, topic, .. } => {
            let config_options = kafka_util::extract_config(&with_options)?;

            consistency = match with_options.remove("consistency") {
                None => Consistency::RealTime,
                Some(Value::String(topic)) => Consistency::BringYourOwn(topic),
                Some(_) => bail!("consistency must be a string"),
            };

            let group_id_prefix = match with_options.remove("group_id_prefix") {
                None => None,
                Some(Value::String(s)) => Some(s),
                Some(_) => bail!("group_id_prefix must be a string"),
            };

            ts_frequency = extract_timestamp_frequency_option(&mut with_options)?;

            // THIS IS EXPERIMENTAL - DO NOT DOCUMENT IT
            // until we have had time to think about what the right UX/design is on a non-urgent timeline!
            // In particular, we almost certainly want the offsets to be specified per-partition.
            // The other major caveat is that by using this feature, you are opting in to
            // not using updates or deletes in CDC sources, and accepting panics if that constraint is violated.
            let start_offset_err = "start_offset must be a nonnegative integer";
            let start_offset = match with_options.remove("start_offset") {
                None => 0,
                Some(Value::Number(n)) => match n.parse::<i64>() {
                    Ok(n) if n >= 0 => n,
                    _ => bail!(start_offset_err),
                },
                Some(_) => bail!(start_offset_err),
            };

            if start_offset != 0 && consistency != Consistency::RealTime {
                bail!("`start_offset` is not yet implemented for BYO consistency sources.")
            }

            let enable_persistence = match with_options.remove("persistence") {
                None => false,
                Some(Value::Boolean(b)) => b,
                Some(_) => bail!("persistence must be a bool!"),
            };

            if enable_persistence && consistency != Consistency::RealTime {
                unsupported!("BYO source persistence")
            }

            let mut start_offsets = HashMap::new();
            start_offsets.insert(0, start_offset);

            let connector = ExternalSourceConnector::Kafka(KafkaSourceConnector {
                addrs: broker.parse()?,
                topic: topic.clone(),
                config_options,
                start_offsets,
                group_id_prefix,
                enable_persistence,
                persisted_files: None,
            });
            let encoding = get_encoding(format)?;
            (connector, encoding)
        }
        Connector::Kinesis { arn, .. } => {
            let arn: ARN = match arn.parse() {
                Ok(arn) => arn,
                Err(e) => bail!("Unable to parse provided ARN: {:#?}", e),
            };
            let stream_name = match arn.resource {
                Resource::Path(path) => {
                    if let Some(path) = path.strip_prefix("stream/") {
                        path.to_owned()
                    } else {
                        bail!("Unable to parse stream name from resource path: {}", path);
                    }
                }
                _ => unsupported!(format!("AWS Resource type: {:#?}", arn.resource)),
            };

            let region: Region = match arn.region {
                Some(region) => match region.parse() {
                    Ok(region) => region,
                    Err(e) => {
                        // Region's fromstr doesn't support parsing custom regions.
                        // If a Kinesis stream's ARN indicates it exists in a custom
                        // region, support it iff a valid endpoint for the stream
                        // is also provided.
                        match with_options.remove("endpoint") {
                            Some(Value::String(endpoint)) => Region::Custom {
                                name: region,
                                endpoint,
                            },
                            _ => bail!(
                                "Unable to parse AWS region: {}. If providing a custom \
                                        region, an `endpoint` option must also be provided",
                                e
                            ),
                        }
                    }
                },
                None => bail!("Provided ARN does not include an AWS region"),
            };

            // todo@jldlaughlin: We should support all (?) variants of AWS authentication.
            // https://github.com/materializeinc/materialize/issues/1991
            let access_key_id = match with_options.remove("access_key_id") {
                Some(Value::String(access_key_id)) => Some(access_key_id),
                Some(_) => bail!("access_key_id must be a string"),
                _ => None,
            };
            let secret_access_key = match with_options.remove("secret_access_key") {
                Some(Value::String(secret_access_key)) => Some(secret_access_key),
                Some(_) => bail!("secret_access_key must be a string"),
                _ => None,
            };
            let token = match with_options.remove("token") {
                Some(Value::String(token)) => Some(token),
                Some(_) => bail!("token must be a string"),
                _ => None,
            };

            let connector = ExternalSourceConnector::Kinesis(KinesisSourceConnector {
                stream_name,
                region,
                access_key_id,
                secret_access_key,
                token,
            });
            let encoding = get_encoding(format)?;
            (connector, encoding)
        }
        Connector::File { path, .. } => {
            let tail = match with_options.remove("tail") {
                None => false,
                Some(Value::Boolean(b)) => b,
                Some(_) => bail!("tail must be a boolean"),
            };
            consistency = match with_options.remove("consistency") {
                None => Consistency::RealTime,
                Some(Value::String(topic)) => Consistency::BringYourOwn(topic),
                Some(_) => bail!("consistency must be a string"),
            };
            ts_frequency = extract_timestamp_frequency_option(&mut with_options)?;

            let connector = ExternalSourceConnector::File(FileSourceConnector {
                path: path.clone().into(),
                tail,
            });
            let encoding = get_encoding(format)?;
            (connector, encoding)
        }
        Connector::AvroOcf { path, .. } => {
            let tail = match with_options.remove("tail") {
                None => false,
                Some(Value::Boolean(b)) => b,
                Some(_) => bail!("tail must be a boolean"),
            };
            consistency = match with_options.remove("consistency") {
                None => Consistency::RealTime,
                Some(Value::String(topic)) => Consistency::BringYourOwn(topic),
                Some(_) => bail!("consistency must be a string"),
            };

            ts_frequency = extract_timestamp_frequency_option(&mut with_options)?;

            let connector = ExternalSourceConnector::AvroOcf(FileSourceConnector {
                path: path.clone().into(),
                tail,
            });
            if format.is_some() {
                bail!("avro ocf sources cannot specify a format");
            }
            let reader_schema = match with_options
                .remove("reader_schema")
                .expect("purification guarantees presence of reader_schema")
            {
                Value::String(s) => s,
                _ => bail!("reader_schema option must be a string"),
            };
            let encoding = DataEncoding::AvroOcf(AvroOcfEncoding { reader_schema });
            (connector, encoding)
        }
    };

    // TODO (materialize#2537): cleanup format validation
    // Avro format validation is different for the Debezium envelope
    // vs the Upsert envelope.
    //
    // For the Debezium envelope, the key schema is not meant to be
    // used to decode records; it is meant to be a subset of the
    // value schema so we can identify what the primary key is.
    //
    // When using the Upsert envelope, we delete the key schema
    // from the value encoding because the key schema is not
    // necessarily a subset of the value schema. Also, we shift
    // the key schema, if it exists, over to the value schema position
    // in the Upsert envelope's key_format so it can be validated like
    // a schema used to decode records.

    // TODO: remove bails as more support for upsert is added.
    let envelope = match &envelope {
        sql_parser::ast::Envelope::None => dataflow_types::Envelope::None,
        sql_parser::ast::Envelope::Debezium => {
            let dedup_strat = match with_options.remove("deduplication") {
                None => DebeziumDeduplicationStrategy::Ordered,
                Some(Value::String(s)) => match s.as_str() {
                    "full" => DebeziumDeduplicationStrategy::Full,
                    "ordered" => DebeziumDeduplicationStrategy::Ordered,
                    _ => bail!("deduplication must be either 'full' or 'ordered'."),
                },
                _ => bail!("deduplication must be either 'full' or 'ordered'."),
            };
            dataflow_types::Envelope::Debezium(dedup_strat)
        }
        sql_parser::ast::Envelope::Upsert(key_format) => match connector {
            Connector::Kafka { .. } => {
                let mut key_encoding = if key_format.is_some() {
                    get_encoding(key_format)?
                } else {
                    encoding.clone()
                };
                match &mut key_encoding {
                    DataEncoding::Avro(AvroEncoding {
                        key_schema,
                        value_schema,
                        ..
                    }) => {
                        if key_schema.is_some() {
                            *value_schema = key_schema.take().unwrap();
                        }
                    }
                    DataEncoding::Bytes | DataEncoding::Text => {}
                    _ => unsupported!("format for upsert key"),
                }
                dataflow_types::Envelope::Upsert(key_encoding)
            }
            _ => unsupported!("upsert envelope for non-Kafka sources"),
        },
        sql_parser::ast::Envelope::CdcV2 => {
            scx.require_experimental_mode("ENVELOPE MATERIALIZE")?;
            if let Connector::AvroOcf { .. } = connector {
                // TODO[btv] - there is no fundamental reason not to support this eventually,
                // but OCF goes through a separate pipeline that it hasn't been implemented for.
                unsupported!("ENVELOPE MATERIALIZE over OCF (Avro files)")
            }
            match format {
                Some(Format::Avro(_)) => {}
                _ => unsupported!("non-Avro-encoded ENVELOPE MATERIALIZE"),
            }
            dataflow_types::Envelope::CdcV2
        }
    };

    if let dataflow_types::Envelope::Upsert(key_encoding) = &envelope {
        match &mut encoding {
            DataEncoding::Avro(AvroEncoding { key_schema, .. }) => {
                *key_schema = None;
            }
            DataEncoding::Bytes | DataEncoding::Text => {
                if let DataEncoding::Avro(_) = &key_encoding {
                    unsupported!("Avro key for this format");
                }
            }
            _ => unsupported!("upsert envelope for this format"),
        }
    }

    let mut desc = encoding.desc(&envelope)?;
    let ignore_source_keys = match with_options.remove("ignore_source_keys") {
        None => false,
        Some(Value::Boolean(b)) => b,
        Some(_) => bail!("ignore_source_keys must be a boolean"),
    };
    if ignore_source_keys {
        desc = desc.without_keys();
    }

    desc = maybe_rename_columns(format!("source {}", name), desc, &col_names)?;

    // TODO(benesch): the available metadata columns should not depend
    // on the format.
    //
    // TODO(brennan): They should not depend on the envelope either. Figure out a way to
    // make all of this more tasteful.
    match (&encoding, &envelope) {
        (DataEncoding::Avro { .. }, _)
        | (DataEncoding::Protobuf { .. }, _)
        | (_, Envelope::Debezium(_)) => (),
        _ => {
            for (name, ty) in external_connector.metadata_columns() {
                desc = desc.with_column(name, ty);
            }
        }
    }

    let if_not_exists = *if_not_exists;
    let materialized = *materialized;
    let name = scx.allocate_name(normalize::object_name(name.clone())?);
    let create_sql = normalize::create_statement(&scx, Statement::CreateSource(stmt))?;

    let source = Source {
        create_sql,
        connector: SourceConnector::External {
            connector: external_connector,
            encoding,
            envelope,
            consistency,
            ts_frequency,
        },
        desc,
    };
    Ok(Plan::CreateSource {
        name,
        source,
        if_not_exists,
        materialized,
    })
}

fn handle_create_table(
    scx: &StatementContext,
    stmt: CreateTableStatement,
) -> Result<Plan, anyhow::Error> {
    let CreateTableStatement {
        name,
        columns,
        constraints,
        with_options,
        if_not_exists,
    } = &stmt;

    if !with_options.is_empty() {
        unsupported!("WITH options");
    }
    if !constraints.is_empty() {
        unsupported!("CREATE TABLE with constraints")
    }

    let names: Vec<_> = columns
        .iter()
        .map(|c| Some(normalize::column_name(c.name.clone())))
        .collect();

    if names.iter().has_duplicates() {
        bail!("cannot CREATE TABLE with duplicate column names");
    }

    // Build initial relation type that handles declared data types
    // and NOT NULL constraints.
    let typ = RelationType::new(
        columns
            .iter()
            .map(|c| {
                let ty = scalar_type_from_sql(&c.data_type)?;
                let mut nullable = true;
                for option in c.options.iter() {
                    match &option.option {
                        ColumnOption::NotNull => nullable = false,
                        other => {
                            unsupported!(format!("CREATE TABLE with column constraint: {}", other))
                        }
                    }
                }
                Ok(ty.nullable(nullable))
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?,
    );

    let name = scx.allocate_name(normalize::object_name(name.clone())?);
    let desc = RelationDesc::new(typ, names);

    let create_sql = normalize::create_statement(&scx, Statement::CreateTable(stmt.clone()))?;
    let table = Table { create_sql, desc };
    Ok(Plan::CreateTable {
        name,
        table,
        if_not_exists: *if_not_exists,
    })
}

/// Renames the columns in `desc` with the names in `column_names` if
/// `column_names` is non-empty.
///
/// Returns an error if the length of `column_names` is not either zero or the
/// arity of `desc`.
fn maybe_rename_columns(
    context: impl fmt::Display,
    desc: RelationDesc,
    column_names: &[Ident],
) -> Result<RelationDesc, anyhow::Error> {
    if column_names.is_empty() {
        return Ok(desc);
    }

    if column_names.len() != desc.typ().column_types.len() {
        bail!(
            "{0} definition names {1} columns, but {0} has {2} columns",
            context,
            column_names.len(),
            desc.typ().column_types.len()
        )
    }

    let new_names = column_names
        .iter()
        .map(|n| Some(normalize::column_name(n.clone())));

    Ok(desc.with_names(new_names))
}

fn handle_drop_database(
    scx: &StatementContext,
    DropDatabaseStatement { name, if_exists }: DropDatabaseStatement,
) -> Result<Plan, anyhow::Error> {
    let name = match scx.resolve_database_ident(name) {
        Ok(name) => name,
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating that the database
            // does not exist.
            //
            // TODO(benesch): adjust the type here so we can more clearly
            // indicate that we don't want to drop any database at all.
            String::new()
        }
        Err(err) => return Err(err.into()),
    };
    Ok(Plan::DropDatabase { name })
}

fn handle_drop_objects(
    scx: &StatementContext,
    DropObjectsStatement {
        object_type,
        if_exists,
        names,
        cascade,
    }: DropObjectsStatement,
) -> Result<Plan, anyhow::Error> {
    match object_type {
        ObjectType::Schema => handle_drop_schema(scx, if_exists, names, cascade),
        ObjectType::Source
        | ObjectType::Table
        | ObjectType::View
        | ObjectType::Index
        | ObjectType::Sink => handle_drop_items(scx, object_type, if_exists, names, cascade),
    }
}

fn handle_drop_schema(
    scx: &StatementContext,
    if_exists: bool,
    names: Vec<ObjectName>,
    cascade: bool,
) -> Result<Plan, anyhow::Error> {
    if names.len() != 1 {
        unsupported!("DROP SCHEMA with multiple schemas");
    }
    match scx.resolve_schema(names.into_element()) {
        Ok((database_spec, schema_spec)) => {
            if let DatabaseSpecifier::Ambient = database_spec {
                bail!(
                    "cannot drop schema {} because it is required by the database system",
                    schema_spec.name
                );
            }
            let mut items = scx.catalog.list_items(&database_spec, &schema_spec.name);
            if !cascade && items.next().is_some() {
                bail!(
                    "schema '{}.{}' cannot be dropped without CASCADE while it contains objects",
                    database_spec,
                    schema_spec.name
                );
            }
            Ok(Plan::DropSchema {
                database_name: database_spec,
                schema_name: schema_spec.name,
            })
        }
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating that the
            // database does not exist.
            // TODO(benesch): adjust the types here properly, rather than making
            // up a nonexistent database.
            Ok(Plan::DropSchema {
                database_name: DatabaseSpecifier::Ambient,
                schema_name: "noexist".into(),
            })
        }
        Err(e) => Err(e.into()),
    }
}

fn handle_drop_items(
    scx: &StatementContext,
    object_type: ObjectType,
    if_exists: bool,
    names: Vec<ObjectName>,
    cascade: bool,
) -> Result<Plan, anyhow::Error> {
    let names = names
        .into_iter()
        .map(|n| scx.resolve_item(n))
        .collect::<Vec<_>>();
    let mut ids = vec![];
    for name in names {
        match name {
            Ok(name) => ids.extend(handle_drop_item(scx, object_type, &name, cascade)?),
            Err(_) if if_exists => {
                // TODO(benesch): generate a notice indicating this
                // item does not exist.
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(Plan::DropItems {
        items: ids,
        ty: object_type,
    })
}

fn handle_drop_item(
    scx: &StatementContext,
    object_type: ObjectType,
    name: &FullName,
    cascade: bool,
) -> Result<Option<GlobalId>, anyhow::Error> {
    let catalog_entry = scx.catalog.get_item(name);
    if catalog_entry.id().is_system() {
        bail!(
            "cannot drop item {} because it is required by the database system",
            name
        );
    }
    if object_type != catalog_entry.item_type() {
        bail!("{} is not of type {}", name, object_type);
    }
    if !cascade {
        for id in catalog_entry.used_by() {
            let dep = scx.catalog.get_item_by_id(id);
            match dep.item_type() {
                CatalogItemType::Table
                | CatalogItemType::Source
                | CatalogItemType::View
                | CatalogItemType::Sink => {
                    bail!(
                        "cannot drop {}: still depended upon by catalog item '{}'",
                        catalog_entry.name(),
                        dep.name()
                    );
                }
                CatalogItemType::Index => (),
            }
        }
    }
    Ok(Some(catalog_entry.id()))
}

fn handle_insert(
    scx: &StatementContext,
    InsertStatement {
        table_name,
        columns,
        source,
    }: InsertStatement,
    params: &Params,
) -> Result<Plan, anyhow::Error> {
    let (id, mut expr) = query::plan_insert_query(scx, table_name, columns, source)?;
    expr.bind_parameters(&params)?;
    let expr = expr.decorrelate();

    Ok(Plan::Insert { id, values: expr })
}

fn handle_select(
    scx: &StatementContext,
    SelectStatement { query, as_of }: SelectStatement,
    params: &Params,
) -> Result<Plan, anyhow::Error> {
    let (relation_expr, _, finishing) = handle_query(scx, query, params, QueryLifetime::OneShot)?;
    let when = match as_of.map(|e| query::eval_as_of(scx, e)).transpose()? {
        Some(ts) => PeekWhen::AtTimestamp(ts),
        None => PeekWhen::Immediately,
    };

    Ok(Plan::Peek {
        source: relation_expr,
        when,
        finishing,
        materialize: true,
    })
}

fn handle_generated_select(scx: &StatementContext, query: String) -> Result<Plan, anyhow::Error> {
    match parse(query)?.into_element() {
        Statement::Select(SelectStatement { query, as_of: _ }) => handle_select(
            scx,
            SelectStatement { query, as_of: None },
            &Params {
                datums: Row::pack(&[]),
                types: vec![],
            },
        ),
        _ => unreachable!("known to be select statement"),
    }
}

fn handle_explain(
    scx: &StatementContext,
    ExplainStatement {
        stage,
        explainee,
        options,
    }: ExplainStatement,
    params: &Params,
) -> Result<Plan, anyhow::Error> {
    let is_view = if let Explainee::View(_) = explainee {
        true
    } else {
        false
    };
    let (scx, query) = match explainee {
        Explainee::View(name) => {
            let full_name = scx.resolve_item(name.clone())?;
            let entry = scx.catalog.get_item(&full_name);
            if entry.item_type() != CatalogItemType::View {
                bail!(
                    "Expected {} to be a view, not a {}",
                    name,
                    entry.item_type(),
                );
            }
            let parsed = crate::parse::parse(entry.create_sql().to_owned())
                .expect("Sql for existing view should be valid sql");
            let query = match parsed.into_last() {
                Statement::CreateView(CreateViewStatement { query, .. }) => query,
                _ => panic!("Sql for existing view should parse as a view"),
            };
            let scx = StatementContext {
                pcx: entry.plan_cx(),
                catalog: scx.catalog,
                param_types: scx.param_types.clone(),
            };
            (scx, query)
        }
        Explainee::Query(query) => (scx.clone(), query),
    };
    // Previouly we would bail here for ORDER BY and LIMIT; this has been relaxed to silently
    // report the plan without the ORDER BY and LIMIT decorations (which are done in post).
    let (mut sql_expr, desc, finishing) =
        query::plan_root_query(&scx, query, QueryLifetime::OneShot)?;
    let finishing = if is_view {
        // views don't use a separate finishing
        sql_expr.finish(finishing);
        None
    } else if finishing.is_trivial(desc.arity()) {
        None
    } else {
        Some(finishing)
    };
    sql_expr.bind_parameters(&params)?;
    let expr = sql_expr.clone().decorrelate();
    Ok(Plan::ExplainPlan {
        raw_plan: sql_expr,
        decorrelated_plan: expr,
        row_set_finishing: finishing,
        stage,
        options,
    })
}

/// Plans and decorrelates a `Query`. Like `query::plan_root_query`, but returns
/// an `::expr::RelationExpr`, which cannot include correlated expressions.
fn handle_query(
    scx: &StatementContext,
    query: Query,
    params: &Params,
    lifetime: QueryLifetime,
) -> Result<(::expr::RelationExpr, RelationDesc, RowSetFinishing), anyhow::Error> {
    let (mut expr, desc, finishing) = query::plan_root_query(scx, query, lifetime)?;
    expr.bind_parameters(&params)?;
    Ok((expr.decorrelate(), desc, finishing))
}

/// Whether a SQL object type can be interpreted as matching the type of the given catalog item.
/// For example, if `v` is a view, `DROP SOURCE v` should not work, since Source and View
/// are non-matching types.
///
/// For now tables are treated as a special kind of source in Materialize, so just
/// allow `TABLE` to refer to either.
impl PartialEq<ObjectType> for CatalogItemType {
    fn eq(&self, other: &ObjectType) -> bool {
        match (self, other) {
            (CatalogItemType::Source, ObjectType::Source)
            | (CatalogItemType::Table, ObjectType::Table)
            | (CatalogItemType::Sink, ObjectType::Sink)
            | (CatalogItemType::View, ObjectType::View)
            | (CatalogItemType::Index, ObjectType::Index) => true,
            (_, _) => false,
        }
    }
}

impl PartialEq<CatalogItemType> for ObjectType {
    fn eq(&self, other: &CatalogItemType) -> bool {
        other == self
    }
}

fn object_type_as_plural_str(object_type: ObjectType) -> &'static str {
    match object_type {
        ObjectType::Schema => "SCHEMAS",
        ObjectType::Index => "INDEXES",
        ObjectType::Table => "TABLES",
        ObjectType::View => "VIEWS",
        ObjectType::Source => "SOURCES",
        ObjectType::Sink => "SINKS",
    }
}

/// Immutable state that applies to the planning of an entire `Statement`.
#[derive(Debug, Clone)]
pub struct StatementContext<'a> {
    pub pcx: &'a PlanContext,
    pub catalog: &'a dyn Catalog,
    /// The types of the parameters in the query. This is filled in as planning
    /// occurs.
    pub param_types: Rc<RefCell<BTreeMap<usize, ScalarType>>>,
}

impl<'a> StatementContext<'a> {
    pub fn allocate_name(&self, name: PartialName) -> FullName {
        FullName {
            database: match name.database {
                Some(name) => DatabaseSpecifier::Name(name),
                None => DatabaseSpecifier::Name(self.catalog.default_database().into()),
            },
            schema: name.schema.unwrap_or_else(|| "public".into()),
            item: name.item,
        }
    }

    pub fn allocate_temporary_name(&self, name: PartialName) -> FullName {
        FullName {
            database: DatabaseSpecifier::Ambient,
            schema: name.schema.unwrap_or_else(|| "mz_temp".to_owned()),
            item: name.item,
        }
    }

    pub fn resolve_default_database(&self) -> Result<DatabaseSpecifier, PlanError> {
        let name = self.catalog.default_database();
        self.catalog.resolve_database(name)?;
        Ok(DatabaseSpecifier::Name(name.into()))
    }

    pub fn resolve_default_schema(&self) -> Result<SchemaSpecifier, PlanError> {
        Ok(self
            .resolve_schema(ObjectName(vec![Ident::new("public")]))?
            .1)
    }

    pub fn resolve_database(&self, name: ObjectName) -> Result<String, PlanError> {
        if name.0.len() != 1 {
            return Err(PlanError::OverqualifiedDatabaseName(name.to_string()));
        }
        self.resolve_database_ident(name.0.into_element())
    }

    pub fn resolve_database_ident(&self, name: Ident) -> Result<String, PlanError> {
        let name = normalize::ident(name);
        self.catalog.resolve_database(&name)?;
        Ok(name)
    }

    pub fn resolve_schema(
        &self,
        mut name: ObjectName,
    ) -> Result<(DatabaseSpecifier, SchemaSpecifier), PlanError> {
        if name.0.len() > 2 {
            return Err(PlanError::OverqualifiedSchemaName(name.to_string()));
        }
        let schema_name = normalize::ident(name.0.pop().unwrap());
        let database_spec = name.0.pop().map(normalize::ident);
        Ok(self.catalog.resolve_schema(database_spec, &schema_name)?)
    }

    pub fn resolve_item(&self, name: ObjectName) -> Result<FullName, PlanError> {
        let name = normalize::object_name(name)?;
        Ok(self.catalog.resolve_item(&name)?)
    }

    pub fn experimental_mode(&self) -> bool {
        self.catalog.experimental_mode()
    }

    pub fn require_experimental_mode(&self, feature_name: &str) -> Result<(), anyhow::Error> {
        if !self.experimental_mode() {
            bail!(
                "{} requires experimental mode; see \
                https://materialize.io/docs/cli/#experimental-mode",
                feature_name
            )
        }
        Ok(())
    }

    pub fn finalize_param_types(self) -> Result<Vec<ScalarType>, anyhow::Error> {
        let param_types = Rc::try_unwrap(self.param_types).unwrap().into_inner();
        let mut out = vec![];
        for (i, (n, typ)) in param_types.into_iter().enumerate() {
            if n != i + 1 {
                bail!("unable to infer type for parameter ${}", i + 1);
            }
            out.push(typ);
        }
        Ok(out)
    }
}
