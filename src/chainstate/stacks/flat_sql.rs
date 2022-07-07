use rusqlite::{Connection, ToSql, OptionalExtension};
use crate::util_lib::db::{tx_begin_immediate, query_row};
use crate::chainstate::stacks::index::{Error, MARFValue, MarfTrieId};
use std::collections::HashMap;
use secp256k1::serde::Serialize;
use std::convert::TryInto;
use stacks_common::types::chainstate::StacksBlockId;

static SQL_DATA_UPDATES_TABLE: &str = "
CREATE TABLE IF NOT EXISTS data_updates (
   block_id INTEGER PRIMARY KEY,
   block_hash TEXT UNIQUE NOT NULL,
   height INTEGER NOT NULL,
   data TEXT NOT NULL,
   old_data TEXT NOT NULL,
   unconfirmed BOOL,
   is_chain_tip BOOL,
);

CREATE INDEX IF NOT EXISTS block_hash_data_updates ON data_updates(block_hash);
";

// TODO: do we want to keep track of block at which last update occurred?
static SQL_KEY_VALUE_STORE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS key_value_store (
   key TEXT UNIQUE NOT NULL,
   value TEXT NOT NULL
);
";

static SQL_EXTENSION_LOCKS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS block_extension_locks (block_hash TEXT PRIMARY KEY);
";

pub fn create_tables_if_needed(conn: &mut Connection) -> Result<(), Error> {
    let tx = tx_begin_immediate(conn)?;

    tx.execute_batch(SQL_DATA_UPDATES_TABLE)?;
    tx.execute_batch(SQL_KEY_VALUE_STORE_TABLE)?;
    tx.execute_batch(SQL_EXTENSION_LOCKS_TABLE)?;

    tx.commit().map_err(|e| e.into())
}

// TODO: maybe implement from_column for MARFValue
pub fn get_value(
    conn: &Connection,
    key: &str,
) -> Result<Option<MARFValue>, Error> {
    let args: &[&dyn ToSql] = &[key];
    let query = "SELECT value FROM key_value_store WHERE key = ?";
    query_row(conn, query, &args)
        .map_err(|e| e.into())
}

pub fn update_key_value_store(
    conn: &Connection,
    key: &str,
    value: &MARFValue,
) -> Result<(), Error> {
    let args: &[&dyn ToSql] = &[key, value];
    let mut s =
        conn.prepare("INSERT INTO key_value_store (key, value) VALUES (?, ?)")?;
    s.execute(args)?;

    Ok(())
}

// QUESTION: should this error if there is no entry for the given block hash?
// TODO: be sure to use this function when chain tip advances
pub fn update_is_chain_tip(
    conn: &Connection,
    block_hash: &MarfTrieId,
    is_chain_tip: bool
) -> Result<(), Error> {
    let args: &[&dyn ToSql] = &[is_chain_tip, &block_id];
    let mut s = conn.prepare("UPDATE data_updates SET is_chain_tip = ? WHERE block_id = ?")?;
    s.execute(args)
        .expect("EXHAUSTION: MARF cannot track more than 2**31 - 1 blocks")?;

    Ok(())
}

pub fn get_chain_tip(
    conn: &Connection
) -> Result<Option<(MarfTrieId, u32)>, Error> {
    let qry = "SELECT (block_hash, height) FROM data_updates WHERE is_chain_tip = ? LIMIT 1";
    query_row(conn, qry, &[&1])
        .map_err(|e| e.into())
}

pub fn remove_old_chain_tip(
    conn: &Connection,
    curr_chain_tip: &MarfTrieId,
) -> Result<(), Error> {
    let args: &[&dyn ToSql] = &[&block_id];
    let mut s = conn.prepare("UPDATE data_updates SET is_chain_tip = 0 WHERE block_id = ?")?;
    s.execute(args)
        .expect("EXHAUSTION: Flat storage cannot track more than 2**31 - 1 blocks");

    Ok(())
}

pub fn add_data_update(
    conn: &Connection,
    block_hash: &MarfTrieId,
    height: u32,
    data_updates: &HashMap<String, MARFValue>,
    old_data: &HashMap<&String, MARFValue>,
    is_chain_tip: bool,
) -> Result<u32, Error> {
    let args: &[&dyn ToSql] = &[block_hash, height, data_updates, old_data, &0, is_chain_tip];
    let mut s =
        conn.prepare("INSERT INTO data_updates (block_hash, height, data, old_data, unconfirmed, is_chain_tip) VALUES (?, ?, ?, ?, ?, ?)")?;
    let block_id = s
        .insert(args)?
        .try_into()
        .expect("EXHAUSTION: Non forking storage is unable to insert data update");

    debug!("Wrote block data updates for {} to rowid {}", block_hash, block_id);
    Ok(block_id)
}

// QUESTION: is the unconfirmed data later deleted or does it stay?
pub fn add_unconfirmed_data_update(
    conn: &Connection,
    block_hash: &MarfTrieId,
    data_updates: &HashMap<String, MARFValue>,
    old_data: &HashMap<String, Option<MARFValue>>,
) -> Result<u32, Error> {
    if let Ok(Some(_)) = get_confirmed_block_identifier(conn, block_hash) {
        panic!("BUG: tried to overwrite confirmed data for {}", block_hash);
    }

    if let Ok(Some(block_id)) = get_unconfirmed_block_identifier(conn, block_hash) {
        let args: &[&dyn ToSql] = &[&data, &block_id];
        let mut s = conn.prepare("UPDATE data_updates SET data = ?, old_data = ? WHERE block_id = ?")?;
        s.execute(args)
            .expect("EXHAUSTION: Flat storage cannot track more than 2**31 - 1 blocks");
    } else {
        let args: &[&dyn ToSql] = &[block_hash, data_updates, old_data, &1, &0];
        let mut s =
            conn.prepare("INSERT INTO data_updates (block_hash, data, old_data, unconfirmed, is_chain_tip) VALUES (?, ?, ?, ?, ?)")?;
        // TODO: check this claim
        s.execute(args)
            .expect("EXHAUSTION: Flat storage cannot track more than 2**31 - 1 blocks");
    }

    let block_id = get_unconfirmed_block_identifier(conn, block_hash)?
        .expect(&format!("BUG: stored {} but got no block ID", block_hash));

    debug!("Wrote unconfirmed data updates for {} to rowid {}", block_hash, block_id);
    Ok(block_id)
}

pub fn get_data_update(
    conn: &Connection,
    block_hash: &MarfTrieId,
) -> Result<Option<HashMap<String, MARFValue>>, Error> {
    let args: &[&dyn ToSql] = &[block_hash];
    let query = "SELECT data FROM data_updates WHERE block_hash = ?";
    query_row(conn, query, &args)
        .map_err(|e| e.into())
}

pub fn get_old_data(
    conn: &Connection,
    block_hash: &MarfTrieId,
) -> Result<Option<HashMap<String, MARFValue>>, Error> {
    let args: &[&dyn ToSql] = &[block_hash];
    let query = "SELECT old_data FROM data_updates WHERE block_hash = ? AND unconfirmed = 0";
    query_row(conn, query, &args)
        .map_err(|e| e.into())
}


// GENERIC GETTERS
pub fn get_block_identifier(conn: &Connection, bhh: &MarfTrieId) -> Result<u32, Error> {
    conn.query_row(
        "SELECT block_id FROM data_updates WHERE block_hash = ?",
        &[bhh],
        |row| row.get("block_id"),
    )
        .map_err(|e| e.into())
}

pub fn get_confirmed_block_identifier(conn: &Connection, bhh: &MarfTrieId) -> Result<Option<u32>, Error> {
    conn.query_row(
        "SELECT block_id FROM data_updates WHERE block_hash = ? and unconfirmed = 0",
        &[bhh],
        |row| row.get("block_id"),
    )
    .optional()
    .map_err(|e| e.into())
}

pub fn get_unconfirmed_block_identifier(conn: &Connection, bhh: &MarfTrieId) -> Result<Option<u32>, Error> {
    conn.query_row(
        "SELECT block_id FROM data_updates WHERE block_hash = ? and unconfirmed = 1",
        &[bhh],
        |row| row.get("block_id"),
    )
    .optional()
    .map_err(|e| e.into())
}

// LOCK FUNCTIONS
pub fn tx_lock_bhh_for_extension(
    tx: &Connection,
    bhh: &T,
    unconfirmed: bool,
) -> Result<bool, Error> {
    if !unconfirmed {
        // confirmed tries can only be extended once.
        // unconfirmed tries can be overwritten.
        let is_bhh_committed = tx
            .query_row(
                "SELECT 1 FROM data_updates WHERE block_hash = ? LIMIT 1",
                &[bhh],
                |_row| Ok(()),
            )
            .optional()?
            .is_some();

        if is_bhh_committed {
            return Ok(false);
        }
    }

    let is_bhh_locked = tx
        .query_row(
            "SELECT 1 FROM block_extension_locks WHERE block_hash = ? LIMIT 1",
            &[bhh],
            |_row| Ok(()),
        )
        .optional()?
        .is_some();
    if is_bhh_locked {
        return Ok(false);
    }

    tx.execute(
        "INSERT INTO block_extension_locks (block_hash) VALUES (?)",
        &[bhh],
    )?;
    Ok(true)
}

pub fn drop_lock(conn: &Connection, bhh: &MarfTrieId) -> Result<(), Error> {
    conn.execute(
        "DELETE FROM block_extension_locks WHERE block_hash = ?",
        &[bhh],
    )?;
    Ok(())
}

pub fn drop_unconfirmed_state(conn: &Connection, bhh: &MarfTrieId) -> Result<(), Error> {
    conn.execute(
        "DELETE FROM data_updates WHERE block_hash = ? and unconfirmed = 1",
        &[bhh]
    )?;
    Ok(())
}

// RE-ORG FUNCTION

// NOTE: should this function error if there is no old data for the given BHH?
fn undo_block_ops(
    conn: &Connection,
    bhh: &MarfTrieId,
) -> Result<(), db_error> {
    // get the old values from the given bhh
    let old_data_opt = get_old_data(conn, bhh)?;
    if let Some(old_data) = old_data_opt {
        for (key, old_value) in old_data {
            update_key_value_store(conn, &key, &new_value)?;
        }
    }

    Ok(())
}

// NOTE: should this function error if there is no old data for the given BHH?
fn redo_block_ops(
    conn: &Connection,
    bhh: &MarfTrieId,
) -> Result<(), db_error> {
    // get the updated values from the given bhh
    let data_opt = get_data_update(conn, bhh)?;
    if let Some(data) = data_opt {
        for (key, new_value) in data {
            update_key_value_store(conn, &key, &new_value)?;
        }
    }

    Ok(())
}

// Tests:
// - test rollbacks of block
// - test re-orgs


// Complications
// - set block hash = impossible without SortitionDB as well