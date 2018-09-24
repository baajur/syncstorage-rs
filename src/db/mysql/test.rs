use std::collections::HashMap;

use db::mysql::{
    models::{run_embedded_migrations, DEFAULT_BSO_TTL},
    pool::MysqlDbPool,
    schema::collections,
};
use db::{error::DbErrorKind, params, util::ms_since_epoch, Sorting};
use env_logger;
use settings::{Secrets, Settings};

use diesel::{
    mysql::MysqlConnection,
    r2d2::{CustomizeConnection, Error as PoolError},
    Connection,
};

// distant future (year 2099) timestamp for tests
pub const MAX_TIMESTAMP: u64 = 4070937600000;

#[derive(Debug)]
pub struct TestTransactionCustomizer;

impl CustomizeConnection<MysqlConnection, PoolError> for TestTransactionCustomizer {
    fn on_acquire(&self, conn: &mut MysqlConnection) -> Result<(), PoolError> {
        conn.begin_test_transaction().map_err(PoolError::QueryError)
    }
}

pub fn pool() -> MysqlDbPool {
    let _ = env_logger::try_init();
    // inherit SYNC_DATABASE_URL from the env
    let settings = Settings::with_env_and_config_file(&None).unwrap();
    let settings = Settings {
        debug: true,
        port: 8000,
        database_url: settings.database_url,
        database_pool_max_size: Some(1),
        database_use_test_transactions: true,
        master_secret: Secrets::default(),
    };

    run_embedded_migrations(&settings).unwrap();
    MysqlDbPool::new(&settings).unwrap()
}

fn pbso(
    user_id: u32,
    cid: i32,
    bid: &str,
    payload: Option<&str>,
    sortindex: Option<i32>,
    ttl: Option<u32>,
) -> params::PutBso {
    params::PutBso {
        user_id,
        collection_id: cid,
        id: bid.to_owned(),
        payload: payload.map(&str::to_owned),
        sortindex,
        ttl,
        modified: ms_since_epoch(),
    }
}

fn gbso(user_id: u32, cid: i32, bid: &str) -> params::GetBso {
    params::GetBso {
        user_id,
        collection_id: cid,
        id: bid.to_owned(),
    }
}

#[test]
fn static_collection_id() {
    let pool = pool();
    let db = pool.get().unwrap();

    // ensure DB actually has predefined common collections
    let cols: Vec<(i32, _)> = vec![
        (1, "clients"),
        (2, "crypto"),
        (3, "forms"),
        (4, "history"),
        (5, "keys"),
        (6, "meta"),
        (7, "bookmarks"),
        (8, "prefs"),
        (9, "tabs"),
        (10, "passwords"),
        (11, "addons"),
        (12, "addresses"),
        (13, "creditcards"),
    ];

    use diesel::{QueryDsl, RunQueryDsl};
    let results: HashMap<i32, String> = collections::table
        .select((collections::id, collections::name))
        .load(&db.conn)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(results.len(), cols.len());
    for (id, name) in &cols {
        assert_eq!(results.get(id).unwrap(), name);
    }

    for (id, name) in &cols {
        let result = db.get_collection_id(name).unwrap();
        assert_eq!(result, *id);
    }

    let cid = db.create_collection("col1").unwrap();
    assert!(cid >= 100);
}

#[test]
fn bso_successfully_updates_single_values() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cid = 1;
    let bid = "testBSO";
    let sortindex = 1;
    let ttl = 3600 * 1000;
    let bso1 = pbso(
        uid,
        cid,
        bid,
        Some("initial value"),
        Some(sortindex),
        Some(ttl),
    );
    db.put_bso_sync(&bso1).unwrap();

    let payload = "Updated payload";
    let bso2 = pbso(uid, cid, bid, Some(payload), None, None);
    db.put_bso_sync(&bso2).unwrap();

    let bso = db.get_bso_sync(&gbso(uid, cid, bid)).unwrap().unwrap();
    assert_eq!(bso.modified, bso2.modified);
    assert_eq!(bso.payload, payload);
    assert_eq!(bso.sortindex, Some(sortindex));
    // XXX: go version assumes ttl was updated here?
    //assert_eq!(bso.expiry, modified + ttl);
    assert_eq!(bso.expiry, bso1.modified + ttl as i64);

    let sortindex = 2;
    let bso2 = pbso(uid, cid, bid, None, Some(sortindex), None);
    db.put_bso_sync(&bso2).unwrap();
    let bso = db.get_bso_sync(&gbso(uid, cid, bid)).unwrap().unwrap();
    assert_eq!(bso.modified, bso2.modified);
    assert_eq!(bso.payload, payload);
    assert_eq!(bso.sortindex, Some(sortindex));
    // XXX:
    //assert_eq!(bso.expiry, modified + ttl);
    assert_eq!(bso.expiry, bso1.modified + ttl as i64);
}

#[test]
fn bso_modified_not_changed_on_ttl_touch() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cid = 1;
    let bid = "testBSO";

    let mut bso1 = pbso(uid, cid, bid, Some("hello"), Some(1), Some(10));
    bso1.modified = ms_since_epoch() - 100;
    db.put_bso_sync(&bso1).unwrap();

    let bso2 = pbso(uid, cid, bid, None, None, Some(15));
    db.put_bso_sync(&bso2).unwrap();
    let bso = db.get_bso_sync(&gbso(uid, cid, bid)).unwrap().unwrap();
    // ttl has changed
    assert_eq!(bso.expiry, bso2.modified + 15);
    // modified has not changed
    assert_eq!(bso.modified, bso1.modified);
}

#[test]
fn put_bso_updates() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cid = 1;
    let bid = "1";
    let bso1 = pbso(uid, cid, bid, Some("initial"), None, None);
    db.put_bso_sync(&bso1).unwrap();

    let bso2 = pbso(uid, cid, bid, Some("Updated"), Some(100), None);
    db.put_bso_sync(&bso2).unwrap();

    let bso = db.get_bso_sync(&gbso(uid, cid, bid)).unwrap().unwrap();
    assert_eq!(Some(bso.payload), bso2.payload);
    assert_eq!(bso.sortindex, bso2.sortindex);
    assert_eq!(bso.modified, bso2.modified);
}

#[test]
fn get_bsos_limit_offset() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cid = 1;
    let size = 12;
    for i in 0..size {
        let mut bso = pbso(
            uid,
            cid,
            &i.to_string(),
            Some(&format!("payload-{}", i)),
            Some(i),
            Some(DEFAULT_BSO_TTL),
        );
        bso.modified += i as i64 * 10;
        db.put_bso_sync(&bso).unwrap();
    }

    let bsos = db
        .get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, 0, Sorting::Index, 0, 0)
        .unwrap();
    assert!(bsos.bsos.is_empty());
    assert!(bsos.more);
    assert_eq!(bsos.offset, 0);

    let bsos = db
        .get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, 0, Sorting::Index, -1, 0)
        .unwrap();
    assert_eq!(bsos.bsos.len(), size as usize);
    assert!(!bsos.more);
    assert_eq!(bsos.offset, 0);

    let newer = 0;
    let limit = 5;
    let offset = 0;
    // XXX: validation?
    /*
    let bsos = db.get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, 0, Sorting::Index, -1, 0).unwrap();
    .. etc
    */

    let bsos = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            newer,
            Sorting::Newest,
            limit,
            offset,
        ).unwrap();
    assert_eq!(bsos.bsos.len(), 5 as usize);
    assert!(bsos.more);
    assert_eq!(bsos.offset, 5);
    assert_eq!(bsos.bsos[0].id, "11");
    assert_eq!(bsos.bsos[4].id, "7");

    let bsos2 = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            newer,
            Sorting::Index,
            limit,
            bsos.offset,
        ).unwrap();
    assert_eq!(bsos2.bsos.len(), 5 as usize);
    assert!(bsos2.more);
    assert_eq!(bsos2.offset, 10);
    assert_eq!(bsos2.bsos[0].id, "6");
    assert_eq!(bsos2.bsos[4].id, "2");

    let bsos3 = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            newer,
            Sorting::Index,
            limit,
            bsos2.offset,
        ).unwrap();
    assert_eq!(bsos3.bsos.len(), 2 as usize);
    assert!(!bsos3.more);
    assert_eq!(bsos3.offset, 0);
    assert_eq!(bsos3.bsos[0].id, "1");
    assert_eq!(bsos3.bsos[1].id, "0");
}

#[test]
fn get_bsos_newer() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cid = 1;
    let modified = ms_since_epoch();
    // XXX: validation
    //db.get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, -1, Sorting::None, 10, 0).is_err()

    for i in (0..=2).rev() {
        let mut pbso = pbso(
            uid,
            cid,
            &format!("b{}", i),
            Some("a"),
            Some(1),
            Some(DEFAULT_BSO_TTL),
        );
        pbso.modified = modified - i;
        db.put_bso_sync(&pbso).unwrap();
    }

    let bsos = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            modified as u64 - 3,
            Sorting::Newest,
            10,
            0,
        ).unwrap();
    assert_eq!(bsos.bsos.len(), 3);
    assert_eq!(bsos.bsos[0].id, "b0");
    assert_eq!(bsos.bsos[1].id, "b1");
    assert_eq!(bsos.bsos[2].id, "b2");

    let bsos = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            modified as u64 - 2,
            Sorting::Newest,
            10,
            0,
        ).unwrap();
    assert_eq!(bsos.bsos.len(), 2);
    assert_eq!(bsos.bsos[0].id, "b0");
    assert_eq!(bsos.bsos[1].id, "b1");

    let bsos = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            modified as u64 - 1,
            Sorting::Newest,
            10,
            0,
        ).unwrap();
    assert_eq!(bsos.bsos.len(), 1);
    assert_eq!(bsos.bsos[0].id, "b0");

    let bsos = db
        .get_bsos_sync(
            uid,
            cid,
            &[],
            MAX_TIMESTAMP,
            modified as u64,
            Sorting::Newest,
            10,
            0,
        ).unwrap();
    assert_eq!(bsos.bsos.len(), 0);
}

#[test]
fn get_bsos_sort() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cid = 1;
    let modified = ms_since_epoch();
    // XXX: validation again
    //db.get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, -1, Sorting::None, 10, 0).is_err()

    for (revi, sortindex) in [1, 0, 2].iter().enumerate().rev() {
        let mut pbso = pbso(
            uid,
            cid,
            &format!("b{}", revi),
            Some("a"),
            Some(*sortindex),
            Some(DEFAULT_BSO_TTL),
        );
        pbso.modified = modified - revi as i64;
        db.put_bso_sync(&pbso).unwrap();
    }

    let bsos = db
        .get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, 0, Sorting::Newest, 10, 0)
        .unwrap();
    assert_eq!(bsos.bsos.len(), 3);
    assert_eq!(bsos.bsos[0].id, "b0");
    assert_eq!(bsos.bsos[1].id, "b1");
    assert_eq!(bsos.bsos[2].id, "b2");

    let bsos = db
        .get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, 0, Sorting::Oldest, 10, 0)
        .unwrap();
    assert_eq!(bsos.bsos.len(), 3);
    assert_eq!(bsos.bsos[0].id, "b2");
    assert_eq!(bsos.bsos[1].id, "b1");
    assert_eq!(bsos.bsos[2].id, "b0");

    let bsos = db
        .get_bsos_sync(uid, cid, &[], MAX_TIMESTAMP, 0, Sorting::Index, 10, 0)
        .unwrap();
    assert_eq!(bsos.bsos.len(), 3);
    assert_eq!(bsos.bsos[0].id, "b2");
    assert_eq!(bsos.bsos[1].id, "b0");
    assert_eq!(bsos.bsos[2].id, "b1");
}

#[test]
fn delete_bsos_in_correct_collection() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let payload = "data";
    db.put_bso_sync(&pbso(uid, 1, "b1", Some(payload), None, None))
        .unwrap();
    db.put_bso_sync(&pbso(uid, 2, "b1", Some(payload), None, None))
        .unwrap();
    db.delete_bsos_sync(uid, 1, &["b1"]).unwrap();
    let bso = db.get_bso_sync(&gbso(uid, 2, "b1")).unwrap();
    assert!(bso.is_some());
}

#[test]
fn get_storage_modified() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    db.create_collection("col1").unwrap();
    let col2 = db.create_collection("col2").unwrap();
    db.create_collection("col3").unwrap();

    let modified = ms_since_epoch() + 100000;
    db.touch_collection(uid, col2, modified).unwrap();

    let m = db.get_storage_modified_sync(uid).unwrap();
    assert_eq!(m, modified);
}

#[test]
fn get_collection_id() {
    let pool = pool();
    let db = pool.get().unwrap();
    db.get_collection_id("bookmarks").unwrap();
}

#[test]
fn create_collection() {
    let pool = pool();
    let db = pool.get().unwrap();

    let name = "NewCollection";
    let cid = db.create_collection(name).unwrap();
    assert_ne!(cid, 0);
    let cid2 = db.get_collection_id(name).unwrap();
    assert_eq!(cid2, cid);
}

#[test]
fn touch_collection() {
    let pool = pool();
    let db = pool.get().unwrap();

    let cid = db.create_collection("test").unwrap();
    db.touch_collection(1, cid, ms_since_epoch()).unwrap();
}

#[test]
fn delete_collection() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let cname = "NewConnection";
    let cid = db.create_collection(cname).unwrap();
    for bid in 1..=3 {
        db.put_bso_sync(&pbso(uid, cid, &bid.to_string(), Some("test"), None, None))
            .unwrap();
    }
    let modified = db.delete_collection_sync(uid, cid).unwrap();
    let modified2 = db.get_storage_modified_sync(uid).unwrap();
    assert_eq!(modified2, modified);

    // make sure BSOs are deleted
    for bid in 1..=3 {
        let result = db.get_bso_sync(&gbso(uid, cid, &bid.to_string())).unwrap();
        assert!(result.is_none());
    }

    let result = db.get_collection_modified_sync(uid, cid);
    match result.unwrap_err().kind() {
        DbErrorKind::CollectionNotFound => assert!(true),
        _ => assert!(false),
    };
}

#[test]
fn get_collections_modified() {
    let pool = pool();
    let db = pool.get().unwrap();

    let uid = 1;
    let name = "test";
    let cid = db.create_collection(name).unwrap();
    let modified = ms_since_epoch();
    db.touch_collection(uid, cid, modified).unwrap();
    let cols = db
        .get_collections_modified_sync(&params::GetCollections { user_id: uid })
        .unwrap();
    assert!(cols.contains_key(name));
    assert_eq!(cols.get(name), Some(&modified));

    let modified = db.get_collection_modified_sync(uid, cid).unwrap();
    assert_eq!(Some(&modified), cols.get(name));
}