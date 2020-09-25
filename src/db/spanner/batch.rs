use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use googleapis_raw::spanner::v1::type_pb::{StructType, Type, TypeCode};
use protobuf::{
    well_known_types::{ListValue, Value},
    RepeatedField,
};
use uuid::Uuid;

use super::support::{null_value, struct_type_field};
use super::{
    models::{Result, SpannerDb, DEFAULT_BSO_TTL, PRETOUCH_TS},
    support::as_value,
};
use crate::{
    db::{params, results, util::to_rfc3339, DbError, DbErrorKind, BATCH_LIFETIME},
    web::extractors::HawkIdentifier,
};

pub async fn create_async(
    db: &SpannerDb,
    params: params::CreateBatch,
) -> Result<results::CreateBatch> {
    let batch_id = Uuid::new_v4().to_simple().to_string();
    let collection_id = db.get_collection_id_async(&params.collection).await?;
    let timestamp = db.timestamp()?.as_i64();

    // Ensure a parent record exists in user_collections before writing to batches
    // (INTERLEAVE IN PARENT user_collections)
    pretouch_collection_async(db, &params.user_id, collection_id).await?;
    let new_batch = results::CreateBatch {
        size: db
            .check_quota(&params.user_id, &params.collection, collection_id)
            .await?,
        id: batch_id,
    };

    db.sql(
        "INSERT INTO batches (fxa_uid, fxa_kid, collection_id, batch_id, expiry)
         VALUES (@fxa_uid, @fxa_kid, @collection_id, @batch_id, @expiry)",
    )?
    .params(params! {
        "fxa_uid" => params.user_id.fxa_uid.clone(),
        "fxa_kid" => params.user_id.fxa_kid.clone(),
        "collection_id" => collection_id.to_string(),
        "batch_id" => new_batch.id.clone(),
        "expiry" => to_rfc3339(timestamp + BATCH_LIFETIME)?,
    })
    .param_types(param_types! {
        "expiry" => TypeCode::TIMESTAMP,
    })
    .execute_dml_async(&db.conn)
    .await?;

    do_append_async(
        db,
        params.user_id,
        collection_id,
        new_batch.clone(),
        params.bsos,
        &params.collection,
    )
    .await?;
    Ok(new_batch)
}

pub async fn validate_async(db: &SpannerDb, params: params::ValidateBatch) -> Result<bool> {
    let exists = get_async(db, params.into()).await?;
    Ok(exists.is_some())
}

pub async fn append_async(db: &SpannerDb, params: params::AppendToBatch) -> Result<()> {
    let mut metrics = db.metrics.clone();
    metrics.start_timer("storage.spanner.append_items_to_batch", None);
    let collection_id = db.get_collection_id_async(&params.collection).await?;

    let current_size = db
        .check_quota(&params.user_id, &params.collection, collection_id)
        .await?;
    let mut batch = params.batch;
    if let Some(size) = current_size {
        batch.size = Some(size + batch.size.unwrap_or(0));
    }

    let exists = validate_async(
        db,
        params::ValidateBatch {
            user_id: params.user_id.clone(),
            collection: params.collection.clone(),
            id: batch.id.clone(),
        },
    )
    .await?;
    if !exists {
        // NOTE: db tests expects this but it doesn't seem necessary w/ the
        // handler validating the batch before appends
        Err(DbErrorKind::BatchNotFound)?
    }

    let collection_id = db.get_collection_id_async(&params.collection).await?;
    do_append_async(
        db,
        params.user_id,
        collection_id,
        batch,
        params.bsos,
        &params.collection,
    )
    .await?;
    Ok(())
}

pub async fn get_async(
    db: &SpannerDb,
    params: params::GetBatch,
) -> Result<Option<results::GetBatch>> {
    let collection_id = db.get_collection_id_async(&params.collection).await?;
    let batch = db
        .sql(
            "SELECT 1
               FROM batches
              WHERE fxa_uid = @fxa_uid
                AND fxa_kid = @fxa_kid
                AND collection_id = @collection_id
                AND batch_id = @batch_id
                AND expiry > CURRENT_TIMESTAMP()",
        )?
        .params(params! {
            "fxa_uid" => params.user_id.fxa_uid.clone(),
            "fxa_kid" => params.user_id.fxa_kid.clone(),
            "collection_id" => collection_id.to_string(),
            "batch_id" => params.id.clone(),
        })
        .execute_async(&db.conn)?
        .one_or_none()
        .await?
        .map(move |_| params::Batch { id: params.id });
    Ok(batch)
}

pub async fn delete_async(db: &SpannerDb, params: params::DeleteBatch) -> Result<()> {
    let collection_id = db.get_collection_id_async(&params.collection).await?;
    // Also deletes child batch_bsos rows (INTERLEAVE IN PARENT batches ON
    // DELETE CASCADE)
    db.sql(
        "DELETE FROM batches
          WHERE fxa_uid = @fxa_uid
            AND fxa_kid = @fxa_kid
            AND collection_id = @collection_id
            AND batch_id = @batch_id",
    )?
    .params(params! {
        "fxa_uid" => params.user_id.fxa_uid.clone(),
        "fxa_kid" => params.user_id.fxa_kid.clone(),
        "collection_id" => collection_id.to_string(),
        "batch_id" => params.id,
    })
    .execute_dml_async(&db.conn)
    .await?;
    Ok(())
}

pub async fn commit_async(
    db: &SpannerDb,
    params: params::CommitBatch,
) -> Result<results::CommitBatch> {
    let mut metrics = db.metrics.clone();
    metrics.start_timer("storage.spanner.apply_batch", None);
    let collection_id = db.get_collection_id_async(&params.collection).await?;

    // Ensure a parent record exists in user_collections before writing to bsos
    // (INTERLEAVE IN PARENT user_collections)
    let timestamp = db
        .update_collection_async(&params.user_id, collection_id, &params.collection)
        .await?;

    let as_rfc3339 = timestamp.as_rfc3339()?;
    {
        // First, UPDATE existing rows in the bsos table with any new values
        // supplied in this batch
        let mut timer2 = db.metrics.clone();
        timer2.start_timer("storage.spanner.apply_batch_update", None);
        db.sql(include_str!("batch_commit_update.sql"))?
            .params(params! {
                "fxa_uid" => params.user_id.fxa_uid.clone(),
                "fxa_kid" => params.user_id.fxa_kid.clone(),
                "collection_id" => collection_id.to_string(),
                "batch_id" => params.batch.id.clone(),
                "timestamp" => as_rfc3339.clone(),
            })
            .param_types(param_types! {
                "timestamp" => TypeCode::TIMESTAMP,
            })
            .execute_dml_async(&db.conn)
            .await?;
    }

    {
        // Then INSERT INTO SELECT remaining rows from this batch into the bsos
        // table (that didn't already exist there)
        let mut timer3 = db.metrics.clone();
        timer3.start_timer("storage.spanner.apply_batch_insert", None);
        db.sql(include_str!("batch_commit_insert.sql"))?
            .params(params! {
                "fxa_uid" => params.user_id.fxa_uid.clone(),
                "fxa_kid" => params.user_id.fxa_kid.clone(),
                "collection_id" => collection_id.to_string(),
                "batch_id" => params.batch.id.clone(),
                "timestamp" => as_rfc3339,
                "default_bso_ttl" => DEFAULT_BSO_TTL.to_string(),
            })
            .param_types(param_types! {
                "timestamp" => TypeCode::TIMESTAMP,
                "default_bso_ttl" => TypeCode::INT64,
            })
            .execute_dml_async(&db.conn)
            .await?;
    }

    delete_async(
        db,
        params::DeleteBatch {
            user_id: params.user_id.clone(),
            collection: params.collection,
            id: params.batch.id,
        },
    )
    .await?;
    // XXX: returning results::PostBsos here isn't needed
    // update the quotas for the user's collection
    db.update_user_collection_quotas(&params.user_id, collection_id)
        .await?;
    Ok(results::PostBsos {
        modified: timestamp,
        success: Default::default(),
        failed: Default::default(),
    })
}

pub async fn do_append_async(
    db: &SpannerDb,
    user_id: HawkIdentifier,
    collection_id: i32,
    batch: results::CreateBatch,
    bsos: Vec<params::PostCollectionBso>,
    collection: &str,
) -> Result<()> {
    // Pass an array of struct objects as @values (for UNNEST), e.g.:
    // [("<fxa_uid>", "<fxa_kid>", 101, "ba1", "bso1", NULL, "payload1", NULL),
    //  ("<fxa_uid>", "<fxa_kid>", 101, "ba1", "bso2", NULL, "payload2", NULL)]
    // https://cloud.google.com/spanner/docs/structs#creating_struct_objects
    let mut running_size: usize = 0;

    // problem: Append may try to insert a duplicate record into the batch_bsos table.
    // this is because spanner doesn't do upserts. at all. Batch_bso is a temp table and
    // items are eventually rolled into bsos.

    fn exist_idx(collection_id: &str, batch_id: &str, bso_id: &str) -> String {
        format!(
            "{collection_id}::{batch_id}::{bso_id}",
            collection_id = collection_id,
            batch_id = batch_id,
            bso_id = bso_id,
        )
    }

    struct UpdateRecord {
        sortindex: Value,
        ttl: Value,
        bso_id: String,
        payload: String,
    };

    //prefetch the existing batch_bsos for this user.
    let mut existing = HashSet::new();
    let mut existing_stream = db
    .sql("SELECT collection_id, batch_id, batch_bso_id from batch_bsos where fxa_uid=@fxa_uid and fxa_kid=@fxa_kid;")?
    .params(params!{
        "fxa_uid" => user_id.fxa_uid.clone(),
        "fxa_kid" => user_id.fxa_kid.clone(),
    }).execute_async(&db.conn)?;
    while let Some(row) = existing_stream.next_async().await {
        let row = row?;
        existing.insert(exist_idx(
            row[0].get_string_value(),
            row[1].get_string_value(),
            row[2].get_string_value(),
        ));
    }

    // Approach 1:
    // iterate and check to see if the record is in batch_bso table already
    let mut insert: Vec<Value> = Vec::new();
    let mut update: Vec<UpdateRecord> = Vec::new();
    for bso in bsos {
        let sortindex = bso
            .sortindex
            .map(|sortindex| as_value(sortindex.to_string()))
            .unwrap_or_else(null_value);
        let payload = bso.payload.map(as_value).unwrap_or_else(null_value);
        if payload != null_value() {
            running_size += payload.get_string_value().len();
        }
        let ttl = bso
            .ttl
            .map(|ttl| as_value(ttl.to_string()))
            .unwrap_or_else(null_value);

        let exist_idx = exist_idx(&collection_id.to_string(), &batch.id, &bso.id);

        if existing.contains(&exist_idx) {
            // need to update this record
            update.push(UpdateRecord {
                sortindex,
                ttl,
                bso_id: bso.id,
                payload: payload.get_string_value().to_owned(),
            });
        } else {
            // convert to a protobuf structure...
            let mut row = ListValue::new();
            row.set_values(RepeatedField::from_vec(vec![
                as_value(user_id.fxa_uid.clone()),
                as_value(user_id.fxa_kid.clone()),
                as_value(collection_id.to_string()),
                as_value(batch.id.clone()),
                as_value(bso.id),
                sortindex,
                payload,
                ttl,
            ]));
            let mut value = Value::new();
            value.set_list_value(row);
            insert.push(value);
            existing.insert(exist_idx);
        };
    }

    if let Some(size) = batch.size {
        if size + running_size >= (db.quota as usize) {
            return Err(db.quota_error(collection));
        }
    }

    let param_types = param_types! {    // ### TODO: this should be normalized to one instance.
        "fxa_uid" => TypeCode::STRING,
        "fxa_kid"=> TypeCode::STRING,
        "collection_id"=> TypeCode::INT64,
        "batch_id"=> TypeCode::STRING,
        "batch_bso_id"=> TypeCode::STRING,
        "sortindex"=> TypeCode::INT64,
        "payload"=> TypeCode::STRING,
        "ttl"=> TypeCode::INT64,
    };
    let fields = param_types
        .clone()
        .into_iter()
        .map(|(name, field_type)| struct_type_field(&name, field_type.get_code()))
        .collect();

    if !insert.is_empty() {
        let mut list_values = ListValue::new();
        list_values.set_values(RepeatedField::from_vec(insert));
        let mut values = Value::new();
        values.set_list_value(list_values);

        // values' type is an ARRAY of STRUCTs
        let mut param_type = Type::new();
        param_type.set_code(TypeCode::ARRAY);
        let mut array_type = Type::new();
        array_type.set_code(TypeCode::STRUCT);

        // STRUCT requires definition of all its field types
        let mut struct_type = StructType::new();
        struct_type.set_fields(RepeatedField::from_vec(fields));
        array_type.set_struct_type(struct_type);
        param_type.set_array_element_type(array_type);

        let mut sqlparams = HashMap::new();
        sqlparams.insert("values".to_owned(), values);
        let mut sqlparam_types = HashMap::new();
        sqlparam_types.insert("values".to_owned(), param_type);
        dbg!("### Doing insert...");
        db.sql(
            "INSERT INTO batch_bsos (fxa_uid, fxa_kid, collection_id, batch_id, batch_bso_id,
                                    sortindex, payload, ttl)
            SELECT * FROM UNNEST(@values)",
        )?
        .params(sqlparams)
        .param_types(sqlparam_types)
        .execute_dml_async(&db.conn)
        .await?;
        dbg!("### done");
    }

    // assuming that "update" is rarer than an insert, we can try using the standard API for that.
    if !update.is_empty() {
        for val in update {
            dbg!("### Updating...");
            db.sql(
                "UPDATE batch_bsos SET sortindex=@sortindex, payload=@payload, ttl=@ttl
                WHERE fxa_uid=@fxa_uid AND fxa_kid=@fxa_kid AND collection_id=@collection_id
                AND batch_id=@batch_id AND batch_bso_id=@batch_bso_id",
            )?
            .params(params!(
                "sortindex" => val.sortindex.get_string_value().to_owned(),
                "payload" => val.payload,
                "ttl" => val.ttl.get_string_value().to_owned(),
                "fxa_uid" => user_id.fxa_uid.clone(),
                "fxa_kid" => user_id.fxa_kid.clone(),
                "collection_id" => collection_id.to_string(),
                "batch_id" => batch.id.clone(),
                "bso_id" => val.bso_id,
            ))
            .param_types(param_types.clone())
            .execute_dml_async(&db.conn)
            .await?;
        }
    }

    Ok(())
}

/// Ensure a parent row exists in user_collections prior to creating a child
/// row in the batches table.
///
/// When no parent exists, a "tombstone" like ("pre birth stone"?) value for
/// modified is inserted, which is explicitly ignored by other queries.
///
/// For the special case of a user creating a batch for a collection with no
/// prior data.
async fn pretouch_collection_async(
    db: &SpannerDb,
    user_id: &HawkIdentifier,
    collection_id: i32,
) -> Result<()> {
    let mut sqlparams = params! {
        "fxa_uid" => user_id.fxa_uid.clone(),
        "fxa_kid" => user_id.fxa_kid.clone(),
        "collection_id" => collection_id.to_string(),
    };
    let result = db
        .sql(
            "SELECT 1
               FROM user_collections
              WHERE fxa_uid = @fxa_uid
                AND fxa_kid = @fxa_kid
                AND collection_id = @collection_id",
        )?
        .params(sqlparams.clone())
        .execute_async(&db.conn)?
        .one_or_none()
        .await?;
    if result.is_none() {
        sqlparams.insert("modified".to_owned(), as_value(PRETOUCH_TS.to_owned()));
        let sql = if db.quota_enabled {
            "INSERT INTO user_collections (fxa_uid, fxa_kid, collection_id, modified, count, total_bytes)
            VALUES (@fxa_uid, @fxa_kid, @collection_id, @modified, 0, 0)"
        } else {
            "INSERT INTO user_collections (fxa_uid, fxa_kid, collection_id, modified)
            VALUES (@fxa_uid, @fxa_kid, @collection_id, @modified)"
        };
        db.sql(sql)?
            .params(sqlparams)
            .param_types(param_types! {
                "modified" => TypeCode::TIMESTAMP,
            })
            .execute_dml_async(&db.conn)
            .await?;
    }
    Ok(())
}

pub fn validate_batch_id(id: &str) -> Result<()> {
    Uuid::from_str(id)
        .map(|_| ())
        .map_err(|e| DbError::internal(&format!("Invalid batch_id: {}", e)))
}
