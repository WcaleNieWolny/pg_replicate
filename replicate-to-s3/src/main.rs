use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    io::Write,
    str::from_utf8,
    time::{Duration, UNIX_EPOCH},
};

use anyhow::anyhow;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{
    config::Credentials,
    primitives::ByteStream,
    types::{Delete, ObjectIdentifier},
    Client,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use clap::Parser;
use futures::StreamExt;
use pg_replicate::{
    EventType, ReplicationClient, ReplicationClientError, ResumptionData, Table, TableSchema,
};
use postgres_protocol::message::backend::{
    BeginBody, CommitBody, LogicalReplicationMessage, RelationBody, ReplicationMessage, Tuple,
    TupleData,
};
use serde::{Deserialize, Serialize};
use serde_cbor::Value;
use tokio_postgres::{
    binary_copy::BinaryCopyOutRow,
    types::{PgLsn, Type},
};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long)]
    s3_username: String,
    #[arg(long)]
    s3_password: String,
    #[arg(long)]
    s3_base_url: String,
    #[arg(long)]
    s3_region: String,
    #[arg(long)]
    s3_bucket_name: String,
    #[arg(long)]
    db_host: String,
    #[arg(long)]
    db_port: u16,
    #[arg(long)]
    db_name: String,
    #[arg(long)]
    db_username: String,
    #[arg(long)]
    db_password: Option<String>,
    #[arg(long)]
    db_slot_name: String,
    #[arg(long)]
    publication_name: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Event {
    event_type: EventType,
    timestamp: DateTime<Utc>,
    relation_id: Option<u32>,
    last_lsn: u64,
    data: Value,
}

const ROWS_PER_DATA_CHUNK: u32 = 10;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let credentials = Credentials::new(
        &args.s3_username,
        &args.s3_password,
        None,
        None,
        "command line",
    );

    let s3_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(&args.s3_base_url)
        .credentials_provider(credentials)
        .region(Region::new(args.s3_region))
        .force_path_style(true)
        .build();

    let s3_client = aws_sdk_s3::Client::from_conf(s3_config);

    let resumption_data = get_relatime_resumption_data(&s3_client, &args.s3_bucket_name).await?;

    let data_chunk_count = resumption_data.as_ref().map(|rd| rd.last_file_name);

    let mut db_client = ReplicationClient::new(
        args.db_host,
        args.db_port,
        args.db_name,
        args.db_username,
        args.db_slot_name,
        resumption_data,
    )
    .await?;

    let schemas = db_client.get_schemas(&args.publication_name).await?;

    let mut relation_id_to_schema = HashMap::new();
    for schema in &schemas {
        relation_id_to_schema.insert(schema.relation_id, schema);
        if !table_copy_done(&s3_client, schema, &args.s3_bucket_name).await? {
            delete_partial_table_copy(&s3_client, schema, &args.s3_bucket_name).await?;
            copy_table(&s3_client, schema, &db_client, &args.s3_bucket_name).await?;
        }
    }

    db_client.commit_txn().await?;

    copy_realtime_changes(
        &s3_client,
        &args.s3_bucket_name,
        &mut db_client,
        &relation_id_to_schema,
        &args.publication_name,
        data_chunk_count,
    )
    .await?;

    Ok(())
}

async fn copy_realtime_changes(
    client: &Client,
    bucket_name: &str,
    repl_client: &mut ReplicationClient,
    rel_id_to_schema: &HashMap<u32, &TableSchema>,
    publication: &str,
    data_chunk_count: Option<u32>,
) -> Result<(), anyhow::Error> {
    let mut row_count: u32 = 0;
    let mut data_chunk_count: u32 = data_chunk_count.unwrap_or(0);
    let logical_stream = repl_client.start_replication_slot(publication).await?;

    tokio::pin!(logical_stream);

    const TIME_SEC_CONVERSION: u64 = 946_684_800;
    let postgres_epoch = UNIX_EPOCH + Duration::from_secs(TIME_SEC_CONVERSION);

    let mut data_chunk_buf = vec![];
    let mut last_lsn = repl_client.consistent_point;
    const REALTIME_CHANGES_PATH_PREFIX: &str = "realtime_changes";

    while let Some(replication_msg) = logical_stream.next().await {
        match replication_msg? {
            ReplicationMessage::XLogData(xlog_data) => {
                let wal_end_lsn: PgLsn = xlog_data.wal_end().into();
                match xlog_data.into_data() {
                    LogicalReplicationMessage::Begin(begin) => {
                        if repl_client.should_skip(wal_end_lsn, EventType::Begin) {
                            continue;
                        }
                        let data = begin_body_to_event_data(&begin);
                        let event_type = EventType::Begin;
                        event_to_cbor(
                            event_type,
                            None,
                            data,
                            &mut data_chunk_buf,
                            wal_end_lsn.into(),
                        )?;
                        if try_save_data_chunk(
                            &mut row_count,
                            &mut data_chunk_count,
                            client,
                            &mut data_chunk_buf,
                            bucket_name,
                            REALTIME_CHANGES_PATH_PREFIX,
                        )
                        .await?
                            && wal_end_lsn != 0.into()
                        {
                            last_lsn = wal_end_lsn
                        }
                    }
                    LogicalReplicationMessage::Commit(commit) => {
                        if repl_client.should_skip(wal_end_lsn, EventType::Commit) {
                            repl_client.stop_skipping_events();
                            continue;
                        }
                        let data = commit_body_to_event_data(&commit);
                        let event_type = EventType::Commit;
                        event_to_cbor(
                            event_type,
                            None,
                            data,
                            &mut data_chunk_buf,
                            wal_end_lsn.into(),
                        )?;
                        if try_save_data_chunk(
                            &mut row_count,
                            &mut data_chunk_count,
                            client,
                            &mut data_chunk_buf,
                            bucket_name,
                            REALTIME_CHANGES_PATH_PREFIX,
                        )
                        .await?
                            && wal_end_lsn != 0.into()
                        {
                            last_lsn = wal_end_lsn;
                        }
                    }
                    LogicalReplicationMessage::Origin(_) => {}
                    LogicalReplicationMessage::Relation(relation) => {
                        if repl_client.should_skip(wal_end_lsn, EventType::Relation) {
                            continue;
                        }
                        match rel_id_to_schema.get(&relation.rel_id()) {
                            Some(schema) => {
                                let data = relation_body_to_event_data(&relation);
                                let event_type = EventType::Relation;
                                event_to_cbor(
                                    event_type,
                                    Some(schema),
                                    data,
                                    &mut data_chunk_buf,
                                    wal_end_lsn.into(),
                                )?;
                                if try_save_data_chunk(
                                    &mut row_count,
                                    &mut data_chunk_count,
                                    client,
                                    &mut data_chunk_buf,
                                    bucket_name,
                                    REALTIME_CHANGES_PATH_PREFIX,
                                )
                                .await?
                                    && wal_end_lsn != 0.into()
                                {
                                    last_lsn = wal_end_lsn;
                                }
                            }
                            None => {
                                return Err(ReplicationClientError::RelationIdNotFound(
                                    relation.rel_id(),
                                ))?;
                            }
                        }
                    }
                    LogicalReplicationMessage::Type(_) => {}
                    LogicalReplicationMessage::Insert(insert) => {
                        if repl_client.should_skip(wal_end_lsn, EventType::Insert) {
                            continue;
                        }
                        match rel_id_to_schema.get(&insert.rel_id()) {
                            Some(schema) => {
                                let data = get_data(schema, insert.tuple());
                                let event_type = EventType::Insert;
                                event_to_cbor(
                                    event_type,
                                    Some(schema),
                                    data,
                                    &mut data_chunk_buf,
                                    wal_end_lsn.into(),
                                )?;
                                if try_save_data_chunk(
                                    &mut row_count,
                                    &mut data_chunk_count,
                                    client,
                                    &mut data_chunk_buf,
                                    bucket_name,
                                    REALTIME_CHANGES_PATH_PREFIX,
                                )
                                .await?
                                    && wal_end_lsn != 0.into()
                                {
                                    last_lsn = wal_end_lsn;
                                }
                            }
                            None => {
                                return Err(ReplicationClientError::RelationIdNotFound(
                                    insert.rel_id(),
                                ))?;
                            }
                        }
                    }
                    LogicalReplicationMessage::Update(update) => {
                        if repl_client.should_skip(wal_end_lsn, EventType::Update) {
                            continue;
                        }
                        match rel_id_to_schema.get(&update.rel_id()) {
                            Some(schema) => {
                                let data = get_data(schema, update.new_tuple());
                                let event_type = EventType::Update;
                                event_to_cbor(
                                    event_type,
                                    Some(schema),
                                    data,
                                    &mut data_chunk_buf,
                                    wal_end_lsn.into(),
                                )?;
                                if try_save_data_chunk(
                                    &mut row_count,
                                    &mut data_chunk_count,
                                    client,
                                    &mut data_chunk_buf,
                                    bucket_name,
                                    REALTIME_CHANGES_PATH_PREFIX,
                                )
                                .await?
                                    && wal_end_lsn != 0.into()
                                {
                                    last_lsn = wal_end_lsn;
                                }
                            }
                            None => {
                                return Err(ReplicationClientError::RelationIdNotFound(
                                    update.rel_id(),
                                ))?;
                            }
                        }
                    }
                    LogicalReplicationMessage::Delete(delete) => {
                        if repl_client.should_skip(wal_end_lsn, EventType::Delete) {
                            continue;
                        }
                        match rel_id_to_schema.get(&delete.rel_id()) {
                            Some(schema) => {
                                let tuple = delete
                                    .key_tuple()
                                    .or(delete.old_tuple())
                                    .expect("no tuple found in delete message");
                                let data = get_data(schema, tuple);
                                let event_type = EventType::Delete;
                                event_to_cbor(
                                    event_type,
                                    Some(schema),
                                    data,
                                    &mut data_chunk_buf,
                                    wal_end_lsn.into(),
                                )?;
                                if try_save_data_chunk(
                                    &mut row_count,
                                    &mut data_chunk_count,
                                    client,
                                    &mut data_chunk_buf,
                                    bucket_name,
                                    REALTIME_CHANGES_PATH_PREFIX,
                                )
                                .await?
                                    && wal_end_lsn != 0.into()
                                {
                                    last_lsn = wal_end_lsn;
                                }
                            }
                            None => {
                                return Err(ReplicationClientError::RelationIdNotFound(
                                    delete.rel_id(),
                                ))?;
                            }
                        }
                    }
                    LogicalReplicationMessage::Truncate(_) => {}
                    msg => {
                        return Err(
                            ReplicationClientError::UnsupportedLogicalReplicationMessage(msg),
                        )?
                    }
                }
            }
            ReplicationMessage::PrimaryKeepAlive(keepalive) => {
                if keepalive.reply() == 1 {
                    let ts = postgres_epoch.elapsed().unwrap().as_micros() as i64;
                    logical_stream
                        .as_mut()
                        .standby_status_update(last_lsn, last_lsn, last_lsn, ts, 0)
                        .await?;
                }
            }
            msg => return Err(ReplicationClientError::UnsupportedReplicationMessage(msg))?,
        }
    }

    Ok(())
}

fn begin_body_to_event_data(begin: &BeginBody) -> Value {
    let mut map = BTreeMap::new();
    map.insert(
        Value::Text("final_lsn".to_string()),
        Value::Integer(begin.final_lsn().into()),
    );
    map.insert(
        Value::Text("timestamp".to_string()),
        Value::Integer(begin.timestamp().into()),
    );
    map.insert(
        Value::Text("xid".to_string()),
        Value::Integer(begin.xid().into()),
    );
    Value::Map(map)
}

fn commit_body_to_event_data(commit: &CommitBody) -> Value {
    let mut map = BTreeMap::new();
    map.insert(
        Value::Text("commit_lsn".to_string()),
        Value::Integer(commit.commit_lsn().into()),
    );
    map.insert(
        Value::Text("end_lsn".to_string()),
        Value::Integer(commit.end_lsn().into()),
    );
    map.insert(
        Value::Text("timestamp".to_string()),
        Value::Integer(commit.timestamp().into()),
    );
    map.insert(
        Value::Text("flags".to_string()),
        Value::Integer(commit.flags().into()),
    );
    Value::Map(map)
}

fn relation_body_to_event_data(relation: &RelationBody) -> Value {
    let schema = relation.namespace().expect("invalid relation namespace");
    let table = relation.name().expect("invalid relation name");
    let cols: Vec<Value> = relation
        .columns()
        .iter()
        .map(|col| {
            let name = col.name().expect("invalid column name");
            let mut map = BTreeMap::new();
            map.insert(
                Value::Text("name".to_string()),
                Value::Text(name.to_string()),
            );
            map.insert(
                Value::Text("identity".to_string()),
                Value::Bool(col.flags() == 1),
            );
            map.insert(
                Value::Text("type_id".to_string()),
                Value::Integer(col.type_id() as i128),
            );
            map.insert(
                Value::Text("type_modifier".to_string()),
                Value::Integer(col.type_modifier() as i128),
            );
            Value::Map(map)
        })
        .collect();
    let mut map = BTreeMap::new();
    map.insert(
        Value::Text("schema".to_string()),
        Value::Text(schema.to_string()),
    );
    map.insert(
        Value::Text("table".to_string()),
        Value::Text(table.to_string()),
    );
    map.insert(Value::Text("columns".to_string()), Value::Array(cols));
    Value::Map(map)
}

fn get_data(table_schema: &TableSchema, tuple: &Tuple) -> Value {
    let data = tuple.tuple_data();
    let mut data_map = BTreeMap::new();
    for (i, attr) in table_schema.attributes.iter().enumerate() {
        let val = get_val_from_tuple_data(&attr.typ, &data[i]);
        data_map.insert(Value::Text(attr.name.clone()), val);
    }
    Value::Map(data_map)
}

fn get_val_from_tuple_data(typ: &Type, val: &TupleData) -> Value {
    let val = match val {
        TupleData::Null => {
            return Value::Null;
        }
        TupleData::UnchangedToast => panic!("unchanged toast"),
        TupleData::Text(bytes) => from_utf8(&bytes[..]).expect("failed to get val"),
    };
    match *typ {
        Type::INT4 => {
            let val: i32 = val.parse().expect("value not i32");
            Value::Integer(val.into())
        }
        Type::VARCHAR => Value::Text(val.to_string()),
        Type::TIMESTAMP => {
            let val = NaiveDateTime::parse_from_str(val, "%Y-%m-%d %H:%M:%S%.f")
                .expect("invalid timestamp");
            Value::Integer(
                val.and_utc()
                    .timestamp_nanos_opt()
                    .expect("failed to get timestamp nanos") as i128,
            )
        }
        ref typ => {
            panic!("unsupported type {typ:?}")
        }
    }
}

const REALTIME_CHANGES_PREFIX: &str = "realtime_changes/";

async fn get_relatime_resumption_data(
    client: &Client,
    bucket_name: &str,
) -> Result<Option<ResumptionData>, anyhow::Error> {
    let Some(last_file_name) =
        largest_realtime_file_number(client, bucket_name, REALTIME_CHANGES_PREFIX).await?
    else {
        return Ok(None);
    };
    let object_prefix = format!("{REALTIME_CHANGES_PREFIX}{last_file_name}");

    let mut last_file = client
        .get_object()
        .bucket(bucket_name)
        .key(object_prefix)
        .send()
        .await?;

    let mut v = vec![];
    while let Some(bytes) = last_file.body.try_next().await? {
        v.write_all(&bytes)?;
    }

    let mut start = 0;
    let mut v = &v[..];
    loop {
        let size: [u8; 8] = (&v[start..start + 8]).try_into()?;
        let size = usize::from_be_bytes(size);
        let new_start = start + 8 + size;
        if v.len() <= new_start {
            v = &v[start + 8..];
            break;
        }
        start = new_start;
    }

    let event: Event = serde_cbor::from_reader(v)?;

    Ok(Some(ResumptionData {
        resume_lsn: event.last_lsn.into(),
        last_event_type: event.event_type,
        last_file_name,
        skipping_events: event.event_type != EventType::Commit,
    }))
}

/// Returns the largest numbered file in with the realtime_changes/
/// prefix.
///
/// This function will get slower over time as the S3 client needs
/// to make multiple calls to get the largest key because in one
/// call S3 API returns maximum 1000 keys.
pub async fn largest_realtime_file_number(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<Option<u32>, anyhow::Error> {
    let mut response = client
        .list_objects_v2()
        .bucket(bucket.to_owned())
        .prefix(prefix)
        .into_paginator()
        .send();

    let mut largest = None;

    while let Some(result) = response.next().await {
        for object in result?.contents() {
            let key = object
                .key()
                .ok_or(anyhow!("missing key"))?
                .strip_prefix(REALTIME_CHANGES_PREFIX)
                .ok_or(anyhow!("wrong prefix"))?;
            let key: u32 = key.parse()?;
            if let Some(last_largest) = largest {
                if key > last_largest {
                    largest = Some(key);
                }
            } else {
                largest = Some(key);
            }
        }
    }

    Ok(largest)
}

async fn table_copy_done(
    client: &Client,
    table_schema: &TableSchema,
    bucket_name: &str,
) -> Result<bool, anyhow::Error> {
    let s3_path = get_done_object_path(&table_schema.table);

    if let Err(e) = client
        .get_object()
        .bucket(bucket_name)
        .key(s3_path)
        .send()
        .await
    {
        match e
            .raw_response()
            .ok_or(anyhow!("no raw response"))?
            .status()
            .is_client_error()
        {
            true => return Ok(false),
            false => (),
        }
    }

    Ok(true)
}

fn get_done_object_path(table: &Table) -> String {
    format!("table_copies/{}.{}/done", table.schema, table.name)
}

async fn mark_table_copy_done(
    table_schema: &TableSchema,
    bucket_name: &str,
    client: &Client,
) -> Result<(), anyhow::Error> {
    let s3_path = get_done_object_path(&table_schema.table);

    client
        .put_object()
        .bucket(bucket_name)
        .key(s3_path)
        .body(ByteStream::from(vec![]))
        .send()
        .await?;

    Ok(())
}

async fn delete_partial_table_copy(
    client: &Client,
    table_schema: &TableSchema,
    bucket_name: &str,
) -> Result<(), anyhow::Error> {
    let s3_prefix = format!(
        "table_copies/{}.{}",
        table_schema.table.schema, table_schema.table.name
    );

    let objects = list_objects(client, bucket_name, &s3_prefix).await?;
    if objects.is_empty() {
        return Ok(());
    }

    client
        .delete_objects()
        .bucket(bucket_name)
        .delete(Delete::builder().set_objects(Some(objects)).build()?)
        .send()
        .await?;
    Ok(())
}

async fn copy_table(
    client: &Client,
    table_schema: &TableSchema,
    repl_client: &ReplicationClient,
    bucket_name: &str,
) -> Result<(), anyhow::Error> {
    let mut row_count: u32 = 0;
    let mut data_chunk_count: u32 = 0;

    let mut data_chunk_buf = vec![];

    let path_prefix = format!(
        "table_copies/{}.{}",
        table_schema.table.schema, table_schema.table.name
    );

    write_table_schema_to_buf(table_schema, &mut data_chunk_buf).await?;
    try_save_data_chunk(
        &mut row_count,
        &mut data_chunk_count,
        client,
        &mut data_chunk_buf,
        bucket_name,
        &path_prefix,
    )
    .await?;

    let types = table_schema
        .attributes
        .iter()
        .map(|attr| attr.typ.clone())
        .collect::<Vec<_>>();
    let rows = repl_client.copy_table(&table_schema.table, &types).await?;
    tokio::pin!(rows);
    while let Some(row) = rows.next().await {
        let row = row?;
        binary_copy_out_row_to_cbor_buf(row, table_schema, &mut data_chunk_buf)?;
        try_save_data_chunk(
            &mut row_count,
            &mut data_chunk_count,
            client,
            &mut data_chunk_buf,
            bucket_name,
            &path_prefix,
        )
        .await?;
    }

    if !data_chunk_buf.is_empty() {
        data_chunk_count += 1;
        let s3_path = format!("{path_prefix}/{}", data_chunk_count);
        save_data_chunk(client, data_chunk_buf.clone(), bucket_name, s3_path).await?;
    }

    mark_table_copy_done(table_schema, bucket_name, client).await?;

    Ok(())
}

fn binary_copy_out_row_to_cbor_buf(
    row: BinaryCopyOutRow,
    table_schema: &TableSchema,
    data_chunk_buf: &mut Vec<u8>,
) -> Result<(), anyhow::Error> {
    let now = Utc::now();
    let mut data_map = BTreeMap::new();
    for (i, attr) in table_schema.attributes.iter().enumerate() {
        let val = get_val_from_row(&attr.typ, &row, i)?;
        data_map.insert(Value::Text(attr.name.clone()), val);
    }
    let event = Event {
        event_type: EventType::Insert,
        timestamp: now,
        relation_id: Some(table_schema.relation_id),
        data: Value::Map(data_map),
        last_lsn: 0,
    };
    let mut event_buf = vec![];
    serde_cbor::to_writer(&mut event_buf, &event)?;
    data_chunk_buf.write_all(&event_buf.len().to_be_bytes())?;
    data_chunk_buf.write_all(&event_buf)?;
    Ok(())
}

fn get_val_from_row(typ: &Type, row: &BinaryCopyOutRow, i: usize) -> Result<Value, anyhow::Error> {
    match *typ {
        Type::INT4 => {
            let val = row.get::<i32>(i);
            Ok(Value::Integer(val as i128))
        }
        Type::VARCHAR => {
            let val = row.get::<&str>(i);
            Ok(Value::Text(val.to_string()))
        }
        Type::TIMESTAMP => {
            let val = row.get::<NaiveDateTime>(i);
            Ok(Value::Integer(
                val.and_utc()
                    .timestamp_nanos_opt()
                    .expect("failed to get timestamp nanos") as i128,
            ))
        }
        ref typ => Err(anyhow::anyhow!("unsupported type {typ:?}")),
    }
}

async fn write_table_schema_to_buf(
    table_schema: &TableSchema,
    data_chunk_buf: &mut Vec<u8>,
) -> Result<(), anyhow::Error> {
    let data = table_schema_to_event_data(table_schema);
    let event_type = EventType::Schema;
    event_to_cbor(event_type, Some(table_schema), data, data_chunk_buf, 0)?;
    Ok(())
}

fn table_schema_to_event_data(table_schema: &TableSchema) -> Value {
    let schema = &table_schema.table.schema;
    let table = &table_schema.table.name;
    let cols = table_schema
        .attributes
        .iter()
        .map(|attribute| {
            let name = attribute.name.to_string();
            let mut map = BTreeMap::new();
            map.insert(
                Value::Text("name".to_string()),
                Value::Text(name.to_string()),
            );
            map.insert(
                Value::Text("identity".to_string()),
                Value::Bool(attribute.identity),
            );
            map.insert(
                Value::Text("nullable".to_string()),
                Value::Bool(attribute.nullable),
            );
            map.insert(
                Value::Text("type_id".to_string()),
                Value::Integer(attribute.typ.oid() as i128),
            );
            map.insert(
                Value::Text("type_modifier".to_string()),
                Value::Integer(attribute.type_modifier as i128),
            );
            Value::Map(map)
        })
        .collect();
    let mut map = BTreeMap::new();
    map.insert(
        Value::Text("schema".to_string()),
        Value::Text(schema.to_string()),
    );
    map.insert(
        Value::Text("table".to_string()),
        Value::Text(table.to_string()),
    );
    map.insert(Value::Text("columns".to_string()), Value::Array(cols));
    Value::Map(map)
}

fn event_to_cbor(
    event_type: EventType,
    table_schema: Option<&TableSchema>,
    data: Value,
    data_chunk_buf: &mut Vec<u8>,
    last_lsn: u64,
) -> Result<(), anyhow::Error> {
    let now = Utc::now();
    let event = Event {
        event_type,
        timestamp: now,
        relation_id: table_schema.map(|ts| ts.relation_id),
        data,
        last_lsn,
    };
    let mut event_buf = vec![];
    serde_cbor::to_writer(&mut event_buf, &event)?;
    data_chunk_buf.write_all(&event_buf.len().to_be_bytes())?;
    data_chunk_buf.write_all(&event_buf)?;
    Ok(())
}

async fn try_save_data_chunk(
    row_count: &mut u32,
    data_chunk_count: &mut u32,
    client: &Client,
    data_chunk_buf: &mut Vec<u8>,
    bucket_name: &str,
    path_prefix: &str,
) -> Result<bool, anyhow::Error> {
    *row_count += 1;
    if *row_count == ROWS_PER_DATA_CHUNK {
        *data_chunk_count += 1;
        let s3_path = format!("{path_prefix}/{data_chunk_count}");
        save_data_chunk(client, data_chunk_buf.clone(), bucket_name, s3_path).await?;
        data_chunk_buf.clear();
        *row_count = 0;
        Ok(true)
    } else {
        Ok(false)
    }
}

async fn save_data_chunk(
    client: &Client,
    data_chunk_buf: Vec<u8>,
    bucket_name: &str,
    path: String,
) -> Result<(), anyhow::Error> {
    let byte_stream = ByteStream::from(data_chunk_buf.clone());
    client
        .put_object()
        .bucket(bucket_name)
        .key(path)
        .body(byte_stream)
        .send()
        .await?;

    Ok(())
}

pub async fn list_objects(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<ObjectIdentifier>, anyhow::Error> {
    let mut response = client
        .list_objects_v2()
        .bucket(bucket.to_owned())
        .prefix(prefix)
        .into_paginator()
        .send();

    let mut objects = vec![];
    while let Some(result) = response.next().await {
        for object in result?.contents() {
            let obj_id = ObjectIdentifier::builder()
                .set_key(Some(
                    object.key().ok_or(anyhow!("missing key"))?.to_string(),
                ))
                .build()?;
            objects.push(obj_id);
        }
    }

    Ok(objects)
}