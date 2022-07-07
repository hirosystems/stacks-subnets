use rusqlite::{Connection, OpenFlags, Transaction};
use std::{fs, io};
use crate::chainstate::stacks::index::{Error, MARFValue, ClarityMarfTrieId, MarfTrieId};
use crate::chainstate::stacks::index::storage::FlushOptions;
use crate::util_lib::db::{sqlite_open, tx_begin_immediate};
use crate::chainstate::stacks::index::storage::SqliteConnection as ConnectionType;
use std::collections::HashMap;
use std::path::PathBuf;
use crate::chainstate::stacks::flat_sql::create_tables_if_needed;
use crate::chainstate::stacks::flat_sql;
use std::ops::{Deref, DerefMut};
use clarity::types::chainstate::StacksBlockId;
use crate::vm::database::ClarityBackingStore;
use clarity::vm::errors::{InterpreterResult, IncomparableError};
use crate::clarity::vm::database::{SpecialCaseHandler, SqliteConnection};
use crate::clarity_vm::special::handle_contract_call_special_cases;
use crate::chainstate::stacks::index::marf::{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, OWN_BLOCK_HEIGHT_KEY};
use stacks_common::consts::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use crate::vm::errors::InterpreterError;
use clarity::vm::database::{HeadersDB, BurnStateDB, ClarityDatabase};
use crate::util::hash::Sha512Trunc256Sum;


#[derive(Clone)]
struct WriteChainTip {
    block_hash: StacksBlockId,
    height: u32,
}

// DISCUSS: delete this struct and implement ClarityBackingStore for FlatFileTx directly?
pub struct WriteableNonForkingStorage<'a> {
    chain_tip: StacksBlockId,
    non_forking_storage: FlatFileTransaction<'a>,
}

// DISCUSS: delete this struct and implement ClarityBackingStore for FlatFileStorage directly?
pub struct ReadOnlyNonForkingStorage {
    chain_tip: StacksBlockId,
    non_forking_storage: FlatFileStorage
}


pub struct FlatFileTransientData {
    uncommitted_writes: Option<(WriteChainTip, HashMap<String, MARFValue>)>,
    unconfirmed: bool,
    readonly: bool,

    // This is the chain tip the storage is at
    // Any data stored in `uncommitted_writes` must build off of this chain tip
    chain_tip: WriteChainTip,
}

// NOTE: NonForkingStorage = MARF + MarfedKV /// FlatFileStorage =  TrieFileStorage
// - Removed notion of MARF since we are removing the "open chain tip" concept
pub struct FlatFileStorage {
    pub db_path: String,
    db: Connection,

    data: FlatFileTransientData,

    // used in testing in order to short-circuit block-height lookups
    //   when the trie struct is tested outside of flat_storage.rs usage
    #[cfg(test)]
    pub test_genesis_block: Option<StacksBlockId>,
}

pub struct FlatFileConnection<'a> {
    pub db_path: &'a str,
    db: ConnectionType<'a>,

    data: &'a mut FlatFileTransientData,

    // used in testing in order to short-circuit block-height lookups
    //   when the trie struct is tested outside of marf.rs usage
    #[cfg(test)]
    pub test_genesis_block: &'a mut Option<StacksBlockId>,
}


// Any storage methods which require a transaction are defined only for this struct; it's db field
// points to a transaction.
pub struct FlatFileTransaction<'a>(FlatFileConnection<'a>);


impl<'a> Deref for FlatFileTransaction<'a> {
    type Target = FlatFileConnection<'a>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl<'a> DerefMut for FlatFileTransaction<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}


impl FlatFileStorage {
    fn setup_db(
        path_str: &str,
        unconfirmed: bool
    ) -> InterpreterResult<FlatFileStorage> {
        let mut path = PathBuf::from(path_str);

        std::fs::create_dir_all(&path)
            .map_err(|_| InterpreterError::FailedToCreateDataDirectory)?;

        path.push("flatfilestorage.sqlite");
        let non_forking_storage_path = path.to_str().ok_or_else(|| InterpreterError::BadFileName)?
            .to_string();

        // Q: there are no forks; make sure init is correct
        let mut flat_file_storage = if unconfirmed {
            FlatFileStorage::open_unconfirmed(&non_forking_storage_path)?
                .map_err(|err| InterpreterError::MarfFailure(err.to_string()))?
        } else {
            FlatFileStorage::open(&non_forking_storage_path)?
                .map_err(|err| InterpreterError::MarfFailure(err.to_string()))?
        };

        if SqliteConnection::check_schema(&flat_file_storage.sqlite_conn()).is_ok() {
            return Ok(flat_file_storage)
        }

        let tx = flat_file_storage
            .storage_tx()
            .map_err(|e| InterpreterError::DBError(e.to_string()))?;
        SqliteConnection::initialize_conn(&tx);
        tx.commit()
            .map_err(|err| InterpreterError::SqliteError(IncomparableError {err}));

        Ok(flat_file_storage)
    }

    // NOTE: similar to open_opts in TrieFileStorage
    fn open_db(
        db_path: &str,
        readonly: bool,
        unconfirmed: bool,
    ) -> Result<FlatFileStorage, Error> {
        let mut create_flag = false;
        let open_flags = if db_path != ":memory:" {
            match fs::metadata(db_path) {
                Err(e) => {
                    if e.kind() == io::ErrorKind::NotFound {
                        // need to create
                        if !readonly {
                            create_flag = true;
                            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                        } else {
                            return Err(Error::NotFoundError);
                        }
                    } else {
                        return Err(Error::IOError(e));
                    }
                }
                Ok(_md) => {
                    // can just open
                    if !readonly {
                        OpenFlags::SQLITE_OPEN_READ_WRITE
                    } else {
                        OpenFlags::SQLITE_OPEN_READ_ONLY
                    }
                }
            }
        } else {
            create_flag = true;
            if !readonly {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_CREATE
            }
        };

        let mut conn = sqlite_open(db_path, open_flags, true)?;
        let db_path = db_path.to_string();

        if create_flag {
            create_tables_if_needed(&mut conn)?;
        }

        debug!(
            "Opened FlatFileStorage {}",
            db_path
        );
        let (block_hash, height) = match flat_sql::get_chain_tip(&db)? {
            None => (StacksBlockId([0; 32]), 0),
            Some((bhh, height)) => (bhh, height)
        };

        let data = FlatFileTransientData {
            uncommitted_writes: None,
            unconfirmed,
            readonly,
            chain_tip: WriteChainTip {
                block_hash,
                height,
            }
        };
        let ret = FlatFileStorage {
            db_path,
            db: conn,
            data,

            #[test]
            test_genesis_block: None,
        };

        Ok(ret)
    }

    pub fn open(db_path: &str) -> Result<FlatFileStorage, Error> {
        FlatFileStorage::open_db(db_path, false, false)
    }

    pub fn open_unconfirmed(db_path: &str) -> Result<FlatFileStorage, Error> {
        FlatFileStorage::open_db(db_path, false, true)
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError);
        }
        if self.unconfirmed() {
            return Err(Error::UnconfirmedError);
        }
        if let Some(_) = self.data.uncommitted_writes.as_ref() {
            let mut tx = self.transaction()?;
            tx.flush()?;
            tx.commit_tx();
        }
        Ok(())
    }

    pub fn make_unconfirmed_chain_tip(chain_tip: &StacksBlockId) -> StacksBlockId {
        let mut bytes = [0u8; 64];
        bytes[0..32].copy_from_slice(chain_tip.as_bytes());
        bytes[32..64].copy_from_slice(chain_tip.as_bytes());

        let h = Sha512Trunc256Sum::from_data(&bytes);
        let mut res_bytes = [0u8; 32];
        res_bytes[0..32].copy_from_slice(h.as_bytes());

        StacksBlockId::from_bytes(&*res_bytes)
    }

    // GETTERS

    pub fn get(&mut self, key: &str) -> Result<Option<MARFValue>, Error> {
        let storage = self.connection()?;
        storage.get_by_key(key)
    }

    pub fn get_block_height_of(
        &mut self,
        bhh: &StacksBlockId,
    ) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height())
        } else {
            FlatFileStorage::get_block_height_miner_tip(
                &self.connection()?,
                bhh
            )
        }
    }

    // NOTE: consolidation - with get_block_at_height
    pub fn get_bhh_at_height(
        &mut self,
        height: u32
    ) -> Result<Option<StacksBlockId>, Error> {
        FlatFileStorage::get_block_at_height(&self.connection()?, height)

    }

    // NOTE: consolidation - with get_bhh_at_height
    pub fn get_block_at_height(
        storage: &FlatFileConnection,
        height: u32
    ) -> Result<Option<StacksBlockId>, Error> {
        #[cfg(test)]
        {
            // used in testing in order to short-circuit block-height lookups
            //   when the trie struct is tested outside of flat_storage.rs usage
            if height == 0 {
                match storage.test_genesis_block {
                    Some(ref s) => return Ok(Some(s.clone())),
                    _ => {}
                }
            }
        }

        let height_key = format!("{}::{}", BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height);
        let marf_value = storage.get_by_key(&height_key)?;
        Ok(marf_value.map(StacksBlockId::from))
    }

    // NOTE: for now, dropping OWN_BLOCK_HEIGHT_KEY for simplicity
    // NOTE: consolidation - get_block_height_of
    pub fn get_block_height_miner_tip(
        storage: &FlatFileConnection,
        block_hash: &StacksBlockId,
    ) -> Result<Option<u32>, Error> {
        #[cfg(test)]
        {
            // used in testing in order to short-circuit block-height lookups
            //   when the trie struct is tested outside of flat_storage.rs usage
            if storage.test_genesis_block.as_ref() == Some(current_block_hash) {
                return Ok(Some(0));
            }
        }

        let hash_key = format!("{}::{}", BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, block_hash);
        let marf_value = storage.get_by_key(&hash_key)?;
        Ok(marf_value.map(u32::from))
    }

    pub fn inner_insert_batch(
        conn: &mut FlatFileTransaction,
        key_value_pairs: Vec<(String, MARFValue)>,
    ) -> Result<(), Error> {
        if key_value_pairs.len() == 0 {
            return Ok(())
        }

        if let Some((_, updates)) = conn.data.uncommitted_writes.as_mut() {
            updates.extend(key_value_pairs);
        }

        Ok(())
    }


    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn unconfirmed(&self) -> bool {
        self.unconfirmed
    }

    pub fn sqlite_tx<'a>(&'a mut self) -> Result<Transaction<'a>, Error> {
        tx_begin_immediate(&mut self.db)
            .map_err(|e| e.into())
    }

    fn sqlite_conn(&self) -> &Connection {
        &self.db
    }

    pub fn transaction(&mut self) -> Result<FlatFileTransaction, Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError);
        }
        let tx = tx_begin_immediate(&mut self.db)?;

        Ok(FlatFileTransaction(FlatFileConnection {
            db: ConnectionType::Tx(tx),
            db_path: &self.db_path,

            data: &mut self.data,
            test_genesis_block: &mut self.test_genesis_block,
        }))
    }

    pub fn connection(&mut self) -> Result<FlatFileConnection, Error> {
        Ok(FlatFileConnection {
            db: ConnectionType::ConnRef(&self.db),
            db_path: &self.db_path,

            data: &mut self.data,
            test_genesis_block: &mut self.test_genesis_block,
        })
    }

    /// Get open chain tip
    pub fn get_open_chain_tip(&self) -> Option<&StacksBlockId> {
        self.data.chain_tip.as_ref().map(|x| &x.block_hash)
    }

    /// Get open chain tip height
    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.data.chain_tip.as_ref().map(|x| x.height)
    }
}


impl FlatFileConnection {
    pub fn readonly(&self) -> bool {
        self.data.readonly.clone()
    }

    pub fn unconfirmed(&self) -> bool {
        self.data.unconfirmed.clone()
    }

    fn get_block_id(&self, block_hash: &StacksBlockId) -> Result<u32, Error> {
        flat_sql::get_block_identifier(&self.db, block_hash)
    }

    pub fn get_by_key(
        &self,
        key: &str,
    ) -> Result<Option<MARFValue>, Error> {
        flat_sql::get_value(&self.db, key)
    }

    pub fn has_confirmed_block(&self, bhh: &StacksBlockId) -> Result<bool, Error> {
        match flat_sql::get_confirmed_block_identifier(&self.db, bhh) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(e)
        }
    }

    pub fn has_unconfirmed_block(&self, bhh: &StacksBlockId) -> Result<bool, Error> {
        match flat_sql::get_unconfirmed_block_identifier(&self.db, bhh) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(e)
        }
    }

    pub fn has_block(&self, bhh: &StacksBlockId) -> Result<bool, Error> {
        Ok(self.has_confirmed_block(bhh)? || self.has_unconfirmed_block(bhh)?)
    }
}

impl<'a, T: MarfTrieId> FlatFileTransaction<'a> {
    pub fn sqlite_tx(&self) -> &Transaction<'a> {
        match &self.db {
            ConnectionType::Tx(ref tx) => tx,
            ConnectionType::ConnRef(_) => {
                unreachable!(
                    "BUG: Constructed FlatFileTransaction with a bare sqlite connection ref"
                );
            }
        }
    }

    // This function stores the data updates for this particular BHH
    // This function also updates the current chain tip in the storage
    fn insert_value_updates(
        &mut self,
        bhh: &StacksBlockId,
        height: u32,
        key_updates: &HashMap<String, MARFValue>
    ) {
        // First obtain the previous key value before inserting update
        let mut old_values = HashMap::new();
        for (key, new_value) in key_updates {
            let old_value = flat_sql::get_value(&self.db, &key)?;
            if let Some(old_value) = old_value {
                old_values.insert(key, old_value);
            }

            flat_sql::update_key_value_store(&self.db, &key, &new_value)?;
        }
        // store key-value updates as aggregate
        flat_sql::add_data_update(&self.db, &bhh, height,&key_updates, &old_values)?;

        flat_sql::drop_lock(&self.db, &bhh)?;

        if let Some(curr_chain_tip) = self.get_open_chain_tip() {
            flat_sql::remove_old_chain_tip(&self.db, curr_chain_tip);
        }

        self.data.chain_tip = WriteChainTip {
            block_hash: bhh.clone(),
            height
        }

        // TODO: store chain tip in flat sql somewhere?
    }

    fn inner_flush(&mut self, flush_options: FlushOptions<'_, T>) -> Result<(), Error> {
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }

        if let Some((WriteChainTip{ block_hash, height}, key_updates)) = self.data.uncommitted_writes.take() {
            // drop_lock is called at the end of each of these branches for the given bhh
            match flush_options {
                FlushOptions::CurrentHeader => self.insert_value_updates(&block_hash, height, &key_updates),
                FlushOptions::NewHeader(real_bhh) => self.insert_value_updates(&real_bhh, height, &key_updates),
                FlushOptions::MinedTable(real_bhh) => {
                    // Not going to write to mined table; this data is unused
                    debug!("Ignoring mined data in call to flush");
                    flat_sql::drop_lock(&self.db, real_bhh);
                }
                FlushOptions::UnconfirmedTable => {
                    if !self.unconfirmed() {
                        return Err(Error::UnconfirmedError)
                    }

                    // Obtain the previous key values
                    let mut old_values = HashMap::new();
                    for (key, new_value) in key_updates {
                        let old_value = flat_sql::get_value(&self.db, &key)?;
                        old_values.insert(key, old_value);
                    }

                    // TODO: fix
                    flat_sql::add_unconfirmed_data_update(&self.db, &block_hash, &key_updates, &old_values);

                    flat_sql::drop_lock(&self.db, &block_hash)?;
                }
            }
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        if self.data.unconfirmed {
            self.inner_flush(FlushOptions::UnconfirmedTable)
        } else {
            self.inner_flush(FlushOptions::CurrentHeader)
        }
    }

    pub fn commit_tx(self) {
        match self.db.as_ref() {
            ConnectionType::Tx(tx) => {
                tx.commit().expect("CORRUPTION: Failed to commit FlatFileStorage");
            }
            ConnectionType::ConnRef(_) => {
                unreachable!(
                    "BUG: Called commit with a bare sqlite connection ref."
                );
            }
        }
    }

    pub fn rollback(self) {
        match self.db.as_ref() {
            ConnectionType::Tx(tx) => {
                tx.rollback().expect("CORRUPTION: Failed to commit MARF");
            }
            ConnectionType::ConnRef(_) => {
                unreachable!("BUG: Constructed FlatFileTransaction with a bare sqlite connection ref");
            }
        }
    }

    pub fn begin(
        &mut self,
        chain_tip: &StacksBlockId,
        next_chain_tip: &StacksBlockId
    ) -> Result<(), Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError)
        }
        if let Some(_) = self.data.uncommitted_writes {
            return Err(Error::InProgressError)
        }
        if self.has_block(next_chain_tip)? {
            error!("Block data already exists: {}", next_chain_tip);
            return Err(Error::ExistsError)
        }
        // check that chain_tip is equal to existing chain_tip
        if chain_tip != self.data.chain_tip.block_hash {
            return Err(Error::NonMatchingForks(chain_tip.clone().to_bytes(), self.data.chain_tip.block_hash.clone().to_bytes()))
        }

        let block_height = self.inner_get_next_height(chain_tip, next_chain_tip)?;
        // switch out storage - extend
        self.extend_to_block(next_chain_tip, block_height)?;
        self.inner_setup_extension(chain_tip, next_chain_tip, block_height, true)?;

        Ok(())
    }

    pub fn begin_unconfirmed(
        &mut self,
        chain_tip: &StacksBlockId
    ) -> Result<StacksBlockId, Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError)
        }
        if let Some(_) = self.data.uncommitted_writes {
            return Err(Error::InProgressError)
        }
        if !self.unconfirmed() {
            return Err(Error::UnconfirmedError)
        }

        // chain_tip must exist and must be confirmed
        if !self.has_confirmed_block(chain_tip)? {
            error!("No such confirmed block {}", chain_tip);
            return Err(Error::NotFoundError)
        }

        let unconfirmed_tip = FlatFileStorage::make_unconfirmed_chain_tip(chain_tip);
        let block_height = self.inner_get_extension_height(chain_tip, unconfirmed_tip);

        let created = self.extend_to_unconfirmed_block(&unconfirmed_tip, block_height)?;
        self.inner_setup_extension(chain_tip, &unconfirmed_tip, block_height, created);

        Ok(unconfirmed_tip)
    }

    fn inner_get_next_height(
        &mut self,
        chain_tip: &StacksBlockId,
        next_chain_tip: &StacksBlockId,
    ) -> Result<u32, Error> {
        let is_parent_sentinel = chain_tip == &StacksBlockId::sentinel();

        if !is_parent_sentinel {
            debug!("Extending off of existing node {}", chain_tip);
        } else {
            debug!("First-ever block {}", next_chain_tip; "block" => %next_chain_tip);
        }

        let block_height = if !is_parent_sentinel {
            let height = FlatFileStorage::get_block_height_miner_tip(&mut self, chain_tip)?
                .ok_or(Error::CorruptionError(format!("Failed to find block height for `{:?}`", chain_tip)))?;
            height.checked_add(1).expect("FATAL: block height overflow!")
        } else {
            0
        };

        Ok(block_height)
    }

    fn inner_setup_extension(
        &mut self,
        chain_tip: &StacksBlockId,
        next_chain_tip: &StacksBlockId,
        block_height: u32,
        new_extension: bool
    ) -> Result<(), Error> {
        if new_extension {
            self.set_block_heights(chain_tip, next_chain_tip, block_height)
                .map_err(|e| {
                    // QUESTION: should I also drop the lock for the bhh here?
                    // Don't think it's necessary since there is a panic if tx is opened unsuccessfully.
                    self.data.uncommitted_writes.take();
                    e
                })?
        }

        Ok(())
    }

    pub fn set_block_heights(
        &mut self,
        block_hash: &StacksBlockId,
        next_block_hash: &StacksBlockId,
        height: u32
    ) -> Result<(), Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError)
        }

        let mut key_value_pairs = vec![];

        let height_key = format!("{}::{}", BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height);
        let hash_key = format!("{}::{}", BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, height);

        debug!(
            "Set {}::{} = {}",
            BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height, next_block_hash
        );
        debug!(
            "Set {}::{} = {}",
            BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, next_block_hash, height
        );
        debug!("Set {} = {}", OWN_BLOCK_HEIGHT_KEY, height);

        key_value_pairs.push((OWN_BLOCK_HEIGHT_KEY.to_string(), MARFValue::from(height)));

        key_value_pairs.push((height_key, MARFValue::from(next_block_hash.clone())));

        key_value_pairs.push((hash_key, MARFValue::from(height)));

        //  DISCUSS: I'm confused by this; it seems redundant
        if height > 0 {
            let prev_height_key = format!("{}::{}", BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, height - 1);
            let prev_hash_key = format!("{}::{}", BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, block_hash);

            debug!(
                "Set {}::{} = {}",
                BLOCK_HEIGHT_TO_HASH_MAPPING_KEY,
                height - 1,
                block_hash
            );
            debug!(
                "Set {}::{} = {}",
                BLOCK_HASH_TO_HEIGHT_MAPPING_KEY,
                block_hash,
                height - 1
            );

            key_value_pairs.push((prev_height_key, MARFValue::from(block_hash.clone())));

            key_value_pairs.push((prev_hash_key, MARFValue::from(height - 1)));
        }

        self.insert_batch(key_value_pairs)?;

        Ok(())
    }

    pub fn extend_to_block(
        &mut self,
        bhh: &StacksBlockId,
        height: u32
    ) -> Result<(), Error> {
        if self.data.readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.data.unconfirmed {
            return Err(Error::UnconfirmedError);
        }

        if self.get_block_id(bhh).is_ok() {
            warn!("Block already exists: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.flush();

        // place a lock on this block, so we can't extend to it again
        if !flat_sql::tx_lock_bhh_for_extension(&self.db, bhh, false)? {
            warn!("Block already extended: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.switch_uncommitted_state(bhh, height, HashMap::new());

        Ok(())
    }

    pub fn extend_to_unconfirmed_block(&mut self, bhh: &StacksBlockId, height: u32) -> Result<bool, Error> {
        if !self.data.unconfirmed {
            return Err(Error::UnconfirmedError)
        }

        let (data_update, created) = if let Some(block_id) = flat_sql::get_unconfirmed_block_identifier(&self.db, bhh)? {
            (flat_sql::get_data_update(&self.db, bhh)?.expect("BUG: unable to retrieve data update that should exist"), false)
        } else {
            (HashMap::new(), true)
        };

        // place a lock on this block, so we can't extend to it again
        if !flat_sql::tx_lock_bhh_for_extension(&self.db, bhh, false)? {
            warn!("Block already extended: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.switch_uncommitted_state(bhh, height,data_update);

        Ok(created)
    }

    /// Switch the uncommitted state with the provided state
    fn switch_uncommitted_state(
        &mut self,
        bhh: &StacksBlockId,
        height: u32,
        data_updates: HashMap<String, MARFValue>
    ) {
        // NOTE: might want to keep track of curr_block & curr_block_id
        self.data.uncommitted_writes.replace((WriteChainTip{ block_hash: bhh.clone(), height }, data_updates));
    }

    pub fn flush_to(&mut self, bhh: &StacksBlockId) -> Result<(), Error> {
        self.inner_flush(FlushOptions::NewHeader(bhh))
    }

    pub fn flush_mined(&mut self, bhh: &StacksBlockId) -> Result<(), Error> {
        self.inner_flush(FlushOptions::MinedTable(bhh))
    }

    /// Drop the uncommitted state
    pub fn drop_extending_state(&mut self) {
        if !self.readonly() {
            if let Some((WriteChainTip {ref block_hash, ..}, _)) = self.data.uncommitted_writes.take() {
                flat_sql::drop_lock(&self.db, block_hash)
                    .expect("Corruption: Failed to drop the extended state lock");
            }
            self.data.uncommitted_writes = None;
        }
    }

    /// Drop the unconfirmed and uncommitted state
    pub fn drop_unconfirmed_state(&mut self, bhh: &StacksBlockId) {
        if !self.data.readonly && self.data.unconfirmed {
            flat_sql::drop_unconfirmed_state(&self.db, bhh)
                .expect("Corruption: Failed to drop unconfirmed state");
            flat_sql::drop_lock(&self.db, bhh)
                .expect("Corruption: Failed to drop the extended state lock");
            self.data.uncommitted_writes = None
        }
    }

    pub fn drop_current(mut self) {
        if !self.readonly() {
            self.drop_extending_state();
            self.rollback();
        }
    }

    pub fn drop_unconfirmed(mut self) {
        if !self.readonly() && self.unconfirmed() {
            if let Some((WriteChainTip {block_hash, height}, _)) = self.data.uncommitted_writes.take() {
                trace!("Dropping unconfirmed data {}", block_hash);
                self.drop_unconfirmed_state(&block_hash);
                // Dropping unconfirmed state cannot be done with a tx rollback,
                //   because the unconfirmed state may already have been written
                //   to the sqlite table before this transaction began
                self.commit_tx();
            } else {
                trace!("drop_unconfirmed() noop");
            }
        }
    }

    /// Get open chain tip
    pub fn get_open_chain_tip(&self) -> Option<&StacksBlockId> {
        self.data.chain_tip.as_ref().map(|x| &x.block_hash)
    }

    /// Get open chain tip height
    pub fn get_open_chain_tip_height(&self) -> Option<u32> {
        self.data.chain_tip.as_ref().map(|x| x.height)
    }

    fn get_block_at_height(&mut self, height: u32) -> Result<Option<StacksBlockId>, Error> {
        FlatFileStorage::get_block_at_height(&self, height)
    }

    // NOTE: if open_chain_tip field is added, can first check that before making query
    pub fn get_block_height_of(&mut self, bhh: &StacksBlockId) -> Result<Option<u32>, Error> {
        if Some(bhh) == self.get_open_chain_tip() {
            return Ok(self.get_open_chain_tip_height());
        } else {
            FlatFileStorage::get_block_height_miner_tip(&mut self, bhh)
        }
    }

    pub fn insert_batch(&mut self, key_value_pairs: Vec<(String, MARFValue)>) -> Result<(), Error> {
        if self.readonly() {
            return Err(Error::ReadOnlyError)
        }

        FlatFileStorage::inner_insert_batch(&mut self, key_value_pairs)
    }
}

impl ReadOnlyNonForkingStorage {
    pub fn as_clarity_db<'b>(
        &mut self,
        headers_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityDatabase<'b> {
        ClarityDatabase::new(self, headers_db, burn_state_db)
    }

}

impl ClarityBackingStore for ReadOnlyNonForkingStorage {
    fn put_all(&mut self, items: Vec<(String, String)>) {
        error!("Attempted to commit changes to read-only FlatFileStorage");
        panic!("BUG: attempted commit to read-only FlatFileStorage");
    }

    fn get(&mut self, key: &str) -> Option<String> {
        self.non_forking_storage.get(key)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!("FlatFileStorage get key {:?}: not found", key);
                    Ok(None)
                }
                _ => Err(e)
            })
            .expect("ERROR: Unexpected FlatFileStorage failure on GET")
            .map(|marf_value| {
                let side_key = marf_value.to_hex();
                trace!("FlatFileStorage get side-key for {:?}: {:?}", key, &side_key);
                SqliteConnection::get(self.get_side_store(), &side_key)
                    .expect (&format!("ERROR: MARF contained value_hash not found in side storage: {}",
                    side_key
                ))
            })
    }

    // NOTE: proofs only used to give proofs in HTTP requests
    fn get_with_proof(&mut self, key: &str) -> Option<(String, Vec<u8>)> {
        unimplemented!()
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.non_forking_storage.get_bhh_at_height(block_height)
            .expect(&format!(
                "Unexpected MARF failure: failed to get block at height {} off of {}.",
                block_height, &self.chain_tip
            ))
            .map(|x| StacksBlockId(x.to_bytes()))
    }

    fn get_current_block_height(&mut self) -> u32 {
        match self.non_forking_storage.get_block_height_of(&self.chain_tip) {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip =
                    StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
                if self.chain_tip == first_tip || self.chain_tip == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.chain_tip
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected FlatFileStorage failure: Failed to get current block height of {}: {:?}",
                    &self.chain_tip, &e
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
        }
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.non_forking_storage
            .get_open_chain_tip_height()
            .expect("Attempted to get the open chain tip from an unopened context.")

    }

    // TODO: there isn't really a "closed" context for flat storage - remove that concept?
    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.non_forking_storage.get_open_chain_tip()
            .expect("Attempted to get the open chain tip from an unopened context.")
            .clone()
    }

    fn get_side_store(&mut self) -> &Connection {
        &self.non_forking_storage.sqlite_conn()
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        Some(&handle_contract_call_special_cases)
    }
}

impl WriteableNonForkingStorage {
    pub fn as_clarity_db<'b>(
        &mut self,
        headers_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityDatabase<'b> {
        ClarityDatabase::new(self, headers_db, burn_state_db)
    }
}

impl ClarityBackingStore for WriteableNonForkingStorage {
    fn put_all(&mut self, items: Vec<(String, String)>) {
        let mut key_value_pairs = Vec::new();
        for (key, value) in items.into_iter() {
            let marf_value = MARFValue::from_value(&value);
            SqliteConnection::put(self.get_side_store(), &marf_value.to_hex(), &value);
            keys.push((key, marf_value));
        }
        self.non_forking_storage.insert_batch(key_value_pairs)
            .expect("ERROR: Unexpected FlatFileStorage failure");
    }

    fn get(&mut self, key: &str) -> Option<String> {
        self.non_forking_storage.get(key)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!(
                        "FlatFileStorage get {:?} off of {:?}: not found",
                        key,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => Err(e)
            })
            .expect("ERROR: Unexpected FlatFileStorage failure on GET")
            .map(|marf_value| {
                let side_key = marf_value.to_hex();
                trace!("FlatFileStorage get side-key for {:?}: {:?}", key, &side_key);
                SqliteConnection::get(self.non_forking_storage.sqlite_tx(), &side_key).expect(&format!(
                    "ERROR: FlatFileStorage contained value_hash not found in side storage: {}",
                    side_key
                ))
            })
    }

    // Function only used in some HTTP requests
    // Not applicable for this type of storage since there are no proofs
    fn get_with_proof(&mut self, key: &str) -> Option<(String, Vec<u8>)> {
        unimplemented!()
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.non_forking_storage.get_block_at_height(height)
            .expect(&format!("Unexpected MARF failure: failed to get block at height {}", height))
    }

    fn get_current_block_height(&mut self) -> u32 {
        match self.non_forking_storage.get_block_height_of(&self.chain_tip) {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip =  StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
                if self.chain_tip == first_tip || self.chain_tip == StacksBlockId([0u8; 32]) {
                    return 0;
                }
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.chain_tip
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected FlatFileStorage failure: Failed to get current block height of {}: {:?}",
                    &self.chain_tip, &e
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
        }
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.non_forking_storage.get_open_chain_tip_height()
            .expect("Attempted to get the open chain tip from an unopened context.")
            .clone()
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.non_forking_storage.get_open_chain_tip()
            .expect("Attempted to get the open chain tip from an unopened context.")
            .clone()
    }

    fn get_side_store(&mut self) -> &Connection {
        &self.non_forking_storage.sqlite_tx()
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        Some(&handle_contract_call_special_cases)
    }

}


// Structs
// - FlatFileConnection - maintains uncommitted writes, unconfirmed + readonly boolean

// Functions: begin, commit, insert_batch, de/serialize blob (jk, should be able to call serialize on HashMap type)

// begin



// Second version considerations
// - Need flush_mined / flush_normal ?
// - want unconfirmed data for short forks?

// TODO
// - clean up errs in flat_sql (compare to other queries)