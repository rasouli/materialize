// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::mem;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use lazy_static::lazy_static;
use rand::Rng;
use regex::{Captures, Regex};
use rusoto_credential::AwsCredentials;
use rusoto_kinesis::{DeleteStreamInput, Kinesis, KinesisClient};
use url::Url;

use repr::strconv;

use crate::error::{Error, InputError, ResultExt};
use crate::parser::{Command, PosCommand, SqlOutput};
use crate::util;

mod avro_ocf;
mod file;
mod kafka;
mod kinesis;
mod sleep;
mod sql;

const DEFAULT_SQL_TIMEOUT: Duration = Duration::from_millis(12700);

/// User-settable configuration parameters.
#[derive(Debug)]
pub struct Config {
    pub kafka_addr: String,
    pub kafka_opts: Vec<(String, String)>,
    pub schema_registry_url: Url,
    pub cert_path: Option<String>,
    pub cert_pass: Option<String>,
    pub aws_region: rusoto_core::Region,
    pub aws_account: String,
    pub aws_credentials: AwsCredentials,
    pub materialized_pgconfig: tokio_postgres::Config,
    pub materialized_catalog_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        const DUMMY_AWS_ACCOUNT: &str = "000000000000";
        const DUMMY_AWS_ACCESS_KEY_ID: &str = "dummy-access-key-id";
        const DUMMY_AWS_SECRET_ACCESS_KEY: &str = "dummy-secret-access-key";
        Config {
            kafka_addr: "localhost:9092".into(),
            kafka_opts: vec![],
            schema_registry_url: "http://localhost:8081".parse().unwrap(),
            cert_path: None,
            cert_pass: None,
            aws_region: rusoto_core::Region::default(),
            aws_account: DUMMY_AWS_ACCOUNT.into(),
            aws_credentials: AwsCredentials::new(
                DUMMY_AWS_ACCESS_KEY_ID,
                DUMMY_AWS_SECRET_ACCESS_KEY,
                None,
                None,
            ),
            materialized_pgconfig: mem::take(
                tokio_postgres::Config::new().host("localhost").port(6875),
            ),
            materialized_catalog_path: None,
        }
    }
}

pub struct State {
    seed: u32,
    temp_dir: tempfile::TempDir,
    materialized_catalog_path: Option<PathBuf>,
    materialized_addr: String,
    pgclient: tokio_postgres::Client,
    schema_registry_url: Url,
    ccsr_client: ccsr::Client,
    kafka_addr: String,
    kafka_admin: rdkafka::admin::AdminClient<rdkafka::client::DefaultClientContext>,
    kafka_admin_opts: rdkafka::admin::AdminOptions,
    kafka_producer: rdkafka::producer::FutureProducer<rdkafka::client::DefaultClientContext>,
    kafka_topics: HashMap<String, i32>,
    aws_region: rusoto_core::Region,
    aws_account: String,
    aws_credentials: AwsCredentials,
    kinesis_client: KinesisClient,
    kinesis_stream_names: Vec<String>,
}

impl State {
    pub async fn reset_materialized(&mut self) -> Result<(), Error> {
        for message in self
            .pgclient
            .simple_query("SHOW DATABASES")
            .await
            .err_ctx("resetting materialized state: SHOW DATABASES".into())?
        {
            if let tokio_postgres::SimpleQueryMessage::Row(row) = message {
                let name = row.get(0).expect("database name is not nullable");
                let query = format!("DROP DATABASE {}", name);
                sql::print_query(&query);
                self.pgclient.batch_execute(&query).await.err_ctx(format!(
                    "resetting materialized state: DROP DATABASE {}",
                    name,
                ))?;
            }
        }
        self.pgclient
            .batch_execute("CREATE DATABASE materialize")
            .await
            .err_ctx("resetting materialized state: CREATE DATABASE materialize".into())?;
        Ok(())
    }

    // Delete the Kinesis streams created for this run of testdrive.
    pub async fn reset_kinesis(&mut self) -> Result<(), Error> {
        if !self.kinesis_stream_names.is_empty() {
            println!(
                "Deleting Kinesis streams {}",
                self.kinesis_stream_names.join(", ")
            );
            for stream_name in &self.kinesis_stream_names {
                self.kinesis_client
                    .delete_stream(DeleteStreamInput {
                        enforce_consumer_deletion: Some(true),
                        stream_name: stream_name.clone(),
                    })
                    .await
                    .map_err(|e| Error::General {
                        ctx: format!("deleting Kinesis stream: {}", stream_name),
                        cause: Some(e.into()),
                        hints: vec![],
                    })?;
            }
        }

        Ok(())
    }
}

pub struct PosAction {
    pub pos: usize,
    pub action: Box<dyn Action + Send + Sync>,
}

#[async_trait]
pub trait Action {
    async fn undo(&self, state: &mut State) -> Result<(), String>;
    async fn redo(&self, state: &mut State) -> Result<(), String>;
}

pub trait SyncAction: Send + Sync {
    fn undo(&self, state: &mut State) -> Result<(), String>;
    fn redo(&self, state: &mut State) -> Result<(), String>;
}

#[async_trait]
impl<T> Action for T
where
    T: SyncAction,
{
    async fn undo(&self, state: &mut State) -> Result<(), String> {
        tokio::task::block_in_place(|| self.undo(state))
    }

    async fn redo(&self, state: &mut State) -> Result<(), String> {
        tokio::task::block_in_place(|| self.redo(state))
    }
}

pub fn build(cmds: Vec<PosCommand>, state: &State) -> Result<Vec<PosAction>, Error> {
    let mut out = Vec::new();
    let mut vars = HashMap::new();
    let mut sql_timeout = DEFAULT_SQL_TIMEOUT;

    vars.insert("testdrive.kafka-addr".into(), state.kafka_addr.clone());
    vars.insert(
        "testdrive.kafka-addr-resolved".into(),
        state
            .kafka_addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "#RESOLUTION-FAILURE#".into()),
    );
    vars.insert(
        "testdrive.schema-registry-url".into(),
        state.schema_registry_url.to_string(),
    );
    vars.insert("testdrive.seed".into(), state.seed.to_string());
    vars.insert(
        "testdrive.temp-dir".into(),
        state.temp_dir.path().display().to_string(),
    );
    {
        let protobuf_descriptors = crate::format::protobuf::gen::FILE_DESCRIPTOR_SET_DATA;
        vars.insert("testdrive.protobuf-descriptors".into(), {
            let mut out = String::new();
            strconv::format_bytes(&mut out, &protobuf_descriptors);
            out
        });
        vars.insert("testdrive.protobuf-descriptors-file".into(), {
            let path = state.temp_dir.path().join("protobuf-descriptors");
            fs::write(&path, &protobuf_descriptors)
                .err_ctx("writing protobuf descriptors file".into())?;
            path.display().to_string()
        });
    }
    vars.insert(
        "testdrive.aws-region".into(),
        state.aws_region.name().to_owned(),
    );
    vars.insert("testdrive.aws-account".into(), state.aws_account.clone());
    vars.insert(
        "testdrive.aws-access-key-id".into(),
        state.aws_credentials.aws_access_key_id().to_owned(),
    );
    vars.insert(
        "testdrive.aws-secret-access-key".into(),
        state.aws_credentials.aws_secret_access_key().to_owned(),
    );
    vars.insert(
        "testdrive.aws-token".into(),
        state
            .aws_credentials
            .token()
            .clone()
            .unwrap_or_else(String::new),
    );
    vars.insert(
        "testdrive.aws-endpoint".into(),
        match &state.aws_region {
            rusoto_core::Region::Custom { endpoint, .. } => endpoint.clone(),
            _ => "".into(),
        },
    );
    for cmd in cmds {
        let pos = cmd.pos;
        let wrap_err = |e| InputError { msg: e, pos };
        let subst = |msg: &str| substitute_vars(msg, &vars).map_err(wrap_err);
        let action: Box<dyn Action + Send + Sync> = match cmd.command {
            Command::Builtin(mut builtin) => {
                for val in builtin.args.values_mut() {
                    *val = subst(val)?;
                }
                for line in &mut builtin.input {
                    *line = subst(line)?;
                }
                match builtin.name.as_ref() {
                    "avro-ocf-write" => Box::new(avro_ocf::build_write(builtin).map_err(wrap_err)?),
                    "avro-ocf-append" => {
                        Box::new(avro_ocf::build_append(builtin).map_err(wrap_err)?)
                    }
                    "avro-ocf-verify" => {
                        Box::new(avro_ocf::build_verify(builtin).map_err(wrap_err)?)
                    }
                    "file-append" => Box::new(file::build_append(builtin).map_err(wrap_err)?),
                    "file-delete" => Box::new(file::build_delete(builtin).map_err(wrap_err)?),
                    "kafka-add-partitions" => {
                        Box::new(kafka::build_add_partitions(builtin).map_err(wrap_err)?)
                    }
                    "kafka-create-topic" => {
                        Box::new(kafka::build_create_topic(builtin).map_err(wrap_err)?)
                    }
                    "kafka-ingest" => Box::new(kafka::build_ingest(builtin).map_err(wrap_err)?),
                    "kafka-verify" => Box::new(kafka::build_verify(builtin).map_err(wrap_err)?),
                    "kinesis-create-stream" => {
                        Box::new(kinesis::build_create_stream(builtin).map_err(wrap_err)?)
                    }
                    "kinesis-update-shards" => {
                        Box::new(kinesis::build_update_shards(builtin).map_err(wrap_err)?)
                    }
                    "kinesis-ingest" => Box::new(kinesis::build_ingest(builtin).map_err(wrap_err)?),
                    "kinesis-verify" => Box::new(kinesis::build_verify(builtin).map_err(wrap_err)?),
                    "set-sql-timeout" => {
                        let duration = builtin.args.string("duration").map_err(wrap_err)?;
                        if duration.to_lowercase() == "default" {
                            sql_timeout = DEFAULT_SQL_TIMEOUT;
                        } else {
                            sql_timeout = parse_duration::parse(&duration)
                                .map_err(|e| wrap_err(e.to_string()))?;
                        }
                        continue;
                    }
                    "set-execution-count" => {
                        // Skip, has already been handled
                        continue;
                    }
                    "random-sleep" => Box::new(sleep::build_sleep(builtin).map_err(wrap_err)?),
                    "set" => {
                        vars.extend(builtin.args);
                        continue;
                    }
                    _ => {
                        return Err(InputError {
                            msg: format!("unknown built-in command {}", builtin.name),
                            pos: cmd.pos,
                        }
                        .into());
                    }
                }
            }
            Command::Sql(mut sql) => {
                sql.query = subst(&sql.query)?;
                if let SqlOutput::Full { expected_rows, .. } = &mut sql.expected_output {
                    for row in expected_rows {
                        for col in row {
                            *col = subst(col)?;
                        }
                    }
                }
                Box::new(sql::build_sql(sql, sql_timeout).map_err(wrap_err)?)
            }
            Command::FailSql(mut sql) => {
                sql.query = subst(&sql.query)?;
                sql.expected_error = subst(&sql.expected_error)?;
                Box::new(sql::build_fail_sql(sql, sql_timeout).map_err(wrap_err)?)
            }
        };
        out.push(PosAction {
            pos: cmd.pos,
            action,
        })
    }
    Ok(out)
}

/// Substituted `${}`-delimited variables from `vars` into `msg`
fn substitute_vars(msg: &str, vars: &HashMap<String, String>) -> Result<String, String> {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"\$\{([^}]+)\}").unwrap();
    }
    let mut err = None;
    let out = RE.replace_all(msg, |caps: &Captures| {
        let name = &caps[1];
        if let Some(val) = vars.get(name) {
            val
        } else {
            err = Some(format!("unknown variable: {}", name));
            "#VAR-MISSING#"
        }
    });
    match err {
        Some(err) => Err(err),
        None => Ok(out.into_owned()),
    }
}

/// Initializes a [`State`] object by connecting to the various external
/// services specified in `config`.
///
/// Returns the initialized `State` and a cleanup future. The cleanup future
/// should be `await`ed only *after* dropping the `State` to check whether any
/// errors occured while dropping the `State`. This awkward API is a workaround
/// for the lack of `AsyncDrop` support in Rust.
pub async fn create_state(
    config: &Config,
) -> Result<(State, impl Future<Output = Result<(), Error>>), Error> {
    let seed = rand::thread_rng().gen();
    let temp_dir = tempfile::tempdir().err_ctx("creating temporary directory".into())?;

    let materialized_catalog_path = if let Some(path) = &config.materialized_catalog_path {
        match fs::metadata(&path) {
            Ok(m) if !m.is_file() => {
                return Err(Error::General {
                    ctx: "materialized catalog path is not a regular file".into(),
                    cause: None,
                    hints: vec![],
                })
            }
            Ok(_) => Some(path.to_path_buf()),
            Err(e) => {
                return Err(Error::General {
                    ctx: "opening materialized catalog path".into(),
                    cause: Some(Box::new(e)),
                    hints: vec![format!("is {} accessible to testdrive?", path.display())],
                })
            }
        }
    } else {
        None
    };

    let (materialized_addr, pgclient, pgconn_task) = {
        let materialized_url = util::postgres::config_url(&config.materialized_pgconfig)?;
        let (pgclient, pgconn) = config
            .materialized_pgconfig
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| Error::General {
                ctx: "opening SQL connection".into(),
                cause: Some(Box::new(e)),
                hints: vec![
                    format!("connection string: {}", materialized_url),
                    "are you running the materialized server?".into(),
                ],
            })?;
        let pgconn_task = tokio::spawn(pgconn).map(|join| {
            join.expect("pgconn_task unexpectedly canceled")
                .map_err(|e| Error::General {
                    ctx: "running SQL connection".into(),
                    cause: Some(Box::new(e)),
                    hints: vec![],
                })
        });
        let materialized_addr = format!(
            "{}:{}",
            materialized_url.host_str().unwrap(),
            materialized_url.port().unwrap()
        );
        (materialized_addr, pgclient, pgconn_task)
    };

    let schema_registry_url = config.schema_registry_url.to_owned();

    let ccsr_client = {
        let mut ccsr_config = ccsr::ClientConfig::new(schema_registry_url.clone());

        if let Some(cert_path) = &config.cert_path {
            let cert = fs::read(cert_path).map_err(|e| Error::General {
                ctx: "reading cert".into(),
                cause: Some(Box::new(e)),
                hints: vec![format!("is {} readable?", cert_path)],
            })?;
            let pass = config.cert_pass.as_deref().unwrap_or("").to_owned();
            let ident =
                ccsr::tls::Identity::from_pkcs12_der(cert, pass).map_err(|e| Error::General {
                    ctx: "reading keystore file as pkcs12".into(),
                    cause: Some(Box::new(e)),
                    hints: vec![format!("is {} a valid pkcs12 file?", cert_path)],
                })?;
            ccsr_config = ccsr_config.identity(ident);
        }

        ccsr_config.build()
    };

    let (kafka_addr, kafka_admin, kafka_admin_opts, kafka_producer, kafka_topics) = {
        use rdkafka::admin::{AdminClient, AdminOptions};
        use rdkafka::client::DefaultClientContext;
        use rdkafka::config::ClientConfig;
        use rdkafka::producer::FutureProducer;

        let mut kafka_config = ClientConfig::new();
        kafka_config.set("bootstrap.servers", &config.kafka_addr);
        if let Some(cert_path) = &config.cert_path {
            kafka_config.set("security.protocol", "ssl");
            kafka_config.set("ssl.keystore.location", cert_path);
            if let Some(cert_pass) = &config.cert_pass {
                kafka_config.set("ssl.keystore.password", cert_pass);
            }
        }
        for (key, value) in &config.kafka_opts {
            kafka_config.set(key, value);
        }

        let admin: AdminClient<DefaultClientContext> =
            kafka_config.create().map_err(|e| Error::General {
                ctx: "opening Kafka connection".into(),
                cause: Some(Box::new(e)),
                hints: vec![format!("connection string: {}", config.kafka_addr)],
            })?;

        let admin_opts = AdminOptions::new().operation_timeout(Some(Duration::from_secs(5)));

        let producer: FutureProducer = kafka_config.create().map_err(|e| Error::General {
            ctx: "opening Kafka producer connection".into(),
            cause: Some(Box::new(e)),
            hints: vec![format!("connection string: {}", config.kafka_addr)],
        })?;

        let topics = HashMap::new();

        (
            config.kafka_addr.to_owned(),
            admin,
            admin_opts,
            producer,
            topics,
        )
    };

    let (aws_region, aws_account, aws_credentials, kinesis_client, kinesis_stream_names) = {
        let kinesis_client = aws_util::kinesis::kinesis_client(
            config.aws_region.clone(),
            Some(config.aws_credentials.aws_access_key_id().to_owned()),
            Some(config.aws_credentials.aws_secret_access_key().to_owned()),
            config.aws_credentials.token().clone(),
        )
        .await
        .map_err(|e| Error::General {
            ctx: "creating Kinesis client".into(),
            cause: Some(e.into()),
            hints: vec![format!("region: {}", config.aws_region.name())],
        })?;
        (
            config.aws_region.clone(),
            config.aws_account.clone(),
            config.aws_credentials.clone(),
            kinesis_client,
            Vec::new(),
        )
    };

    let state = State {
        seed,
        temp_dir,
        materialized_catalog_path,
        materialized_addr,
        pgclient,
        schema_registry_url,
        ccsr_client,
        kafka_addr,
        kafka_admin,
        kafka_admin_opts,
        kafka_producer,
        kafka_topics,
        aws_region,
        aws_account,
        aws_credentials,
        kinesis_client,
        kinesis_stream_names,
    };
    Ok((state, pgconn_task))
}
