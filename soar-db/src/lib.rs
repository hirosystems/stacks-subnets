//! (S)ubnets (O)ptimistic (A)daptive (R)eplay DB
//!
//! The SoarDB is an optimistic fork-aware data store (a replacement
//! for the MARF used in stacks-blockchain).
//!
//! The general idea with the datastore is to store the current data
//! view as a normal key-value store and track the history of
//! operations on the storage. When a fork occurs, the data state is
//! unwound and then replayed.

extern crate clarity;
extern crate stacks_common;
#[macro_use(o, slog_log, slog_trace, slog_debug, slog_info, slog_warn, slog_error)]
extern crate slog;
extern crate slog_json;
extern crate slog_term;

use std::collections::HashMap;

use crate::memory::MemoryBackingStore;
use clarity::{
    util::hash::Sha512Trunc256Sum,
    vm::{
        database::{BurnStateDB, ClarityBackingStore, ClarityDatabase, HeadersDB},
        errors::InterpreterResult,
        types::QualifiedContractIdentifier,
    },
};
use stacks_common::types::chainstate::StacksBlockId;

pub mod memory;

#[cfg(test)]
pub mod tests;

pub trait SoarBackingStore {}

/// SoarDB is a key-value store where operations
///  are applied in blocks. Each block (except the *sole*
///  genesis block) has a parent block. The SoarDB views
///  its data from the perspective of a `current_block` value.
///
/// SoarDB handles re-organizations of the database view through
///  invocations of `set_block()`. When this method is invoked, the
///  database applies any rollbacks or re-application of operations
///  necessary to change the view.
pub struct SoarDB {
    storage: MemoryBackingStore,
}

pub struct PendingSoarBlock<'a> {
    db: &'a mut SoarDB,
    parent_block: StacksBlockId,
    id: StacksBlockId,
    /// This vec *only* grows: once operations are applied to the
    ///  pending block, the block either must be abandoned or
    ///  committed with those operations.
    ///
    /// Individual transaction rollbacks are handled by the Clarity KV
    /// wrapper.
    pending_ops: Vec<PutCommand>,
    pending_view: HashMap<String, String>,
    is_unconfirmed: bool,
}

pub struct ReadOnlySoarConn<'a> {
    db: &'a SoarDB,
}

#[derive(Clone)]
/// Captures a key-value store's "put" operation, but is
/// *reversible*. The `prior_value` field stores the entry
/// being written over.
pub struct PutCommand {
    key: String,
    /// If a prior value existed for this entry, store it here
    /// If this is the first value for the key, this will be None
    prior_value: Option<String>,
    value: String,
}

/// Error types for SoarDB
#[derive(PartialEq, Debug)]
pub enum SoarError {
    /// The db cannot find the parent of a block in its storage. More
    /// context is supplied in the error's string message.
    NoParentBlock(&'static str),
    /// The given stacks block could not be found in storage.
    BlockNotFound(StacksBlockId),
    /// The caller attempted an operation that would write a genesis block,
    /// but this database already stored a genesis block.
    GenesisRewriteAttempted,
    /// Adding the block would overflow the block height parameter
    BlockHeightOverflow,
    /// The db failed to rollback successfully, reaching an unexpected DB state.
    /// This error *should never* occur under normal operation.
    MismatchViewDuringRollback,
    /// The db failed to rollback successfully, reaching the genesis block before
    ///  finding a fork point.
    /// This error *should never* occur under normal operation.
    RollbackBeyondGenesis,
    /// The DB's fork view must be altered before the requested operation
    ViewChangeRequired,
}

impl SoarDB {
    pub fn new_memory() -> SoarDB {
        SoarDB {
            storage: MemoryBackingStore::new(),
        }
    }

    /// If the DB has a block, then the current block should be returned
    /// If there is *no* block data yet, this will return none
    pub fn current_block(&self) -> Option<&StacksBlockId> {
        self.storage.current_block()
    }

    /// Get a value from the key-value store from the view of `current_block()`
    pub fn get_value(&self, key: &str) -> Result<Option<String>, SoarError> {
        self.storage.get_value(key)
    }

    pub fn get_block_height(&self, block: &StacksBlockId) -> Result<u64, SoarError> {
        self.storage.get_block_height(block)
    }

    /// Retarget the db to `block`, performing any unrolls or replays required to do so
    pub fn set_block(&mut self, block: &StacksBlockId) -> Result<(), SoarError> {
        // already pointed at the block, just return
        if self.current_block() == Some(block) {
            return Ok(());
        }

        // this block requires a rollback!
        // Step 1: find the "fork point", which is the most recent common ancestor
        //         of `block` and `current_block()`
        //
        //  We will do this by using the *block height* to walk backwards until the
        //   two ancestor paths meet. First, we find ancestors at the same height,
        //   then we loop until the ancestors are equal.

        if !self.storage.has_block(block) {
            return Err(SoarError::BlockNotFound(block.clone()));
        }

        // unwrap is safe, because current_block().is_none() is checked in branch above
        let mut ancestor_a = self
            .current_block()
            .ok_or_else(|| SoarError::RollbackBeyondGenesis)?
            .clone();
        let mut ancestor_b = block.clone();
        let mut ancestor_a_ht = self.storage.get_block_height(&ancestor_a)?;
        let mut ancestor_b_ht = self.storage.get_block_height(&ancestor_b)?;

        // we track the path of "ancestor b" so that we know what needs to be
        //  applied to get from the `fork_point` to `block`
        let mut ancestors_b = vec![block.clone()];

        while ancestor_a_ht != ancestor_b_ht {
            if ancestor_a_ht > ancestor_b_ht {
                (ancestor_a, ancestor_a_ht) = self.get_block_parent(&ancestor_a, ancestor_a_ht)?;
            } else {
                (ancestor_b, ancestor_b_ht) = self.get_block_parent(&ancestor_b, ancestor_b_ht)?;
                ancestors_b.push(ancestor_b.clone());
            }
        }

        while ancestor_a != ancestor_b {
            (ancestor_a, ancestor_a_ht) = self.get_block_parent(&ancestor_a, ancestor_a_ht)?;
            (ancestor_b, ancestor_b_ht) = self.get_block_parent(&ancestor_b, ancestor_b_ht)?;
        }

        let fork_point = ancestor_a;

        // fix the ancestors_b list so that it contains all the blocks
        //  that need to be applied starting from `fork_point` to
        //  reach `block`. To do this, we check if the tail of the list is equal
        //  to the `fork_point`, and if so, remove it. This could result in a zero-length
        //  list if `block` == `fork_point`.
        if ancestors_b.last() == Some(&fork_point) {
            ancestors_b.pop();
        }

        // Now, we have the most recent common ancestor (ancestor_a == ancestor_b)
        // We can now execute Step 2: undo from the current block to the common ancestor
        let mut current_block = self
            .current_block()
            .ok_or_else(|| SoarError::RollbackBeyondGenesis)?
            .clone();
        while &current_block != &fork_point {
            self.storage.undo_block(&current_block)?;
            current_block = self
                .current_block()
                .ok_or_else(|| SoarError::RollbackBeyondGenesis)?
                .clone();
        }

        // Step 3: apply all the blocks from `fork_point` through to `block`, and then
        //  apply the new block
        for block_to_apply in ancestors_b.iter().rev() {
            current_block = self
                .current_block()
                .ok_or_else(|| SoarError::RollbackBeyondGenesis)?
                .clone();
            let parent_block = self.storage.get_block_parent(block_to_apply)?;
            assert_eq!(
                current_block, parent_block,
                "Failed while replaying operations: expected parent and current block to align"
            );
            self.storage.reapply_block(block_to_apply)?;
        }

        current_block = self
            .current_block()
            .ok_or_else(|| SoarError::RollbackBeyondGenesis)?
            .clone();
        assert_eq!(
            &current_block, block,
            "Failed while replaying operations: expected current block to align to block"
        );

        Ok(())
    }

    /// Fetch the parent of `block` and its block height, checking that it matches `block_ht - 1`
    fn get_block_parent(
        &self,
        block: &StacksBlockId,
        block_ht: u64,
    ) -> Result<(StacksBlockId, u64), SoarError> {
        if block_ht == 0 {
            return Err(SoarError::NoParentBlock("No parent at zero-block"));
        }
        let parent = self.storage.get_block_parent(&block)?;
        let parent_ht = self.storage.get_block_height(&parent)?;
        assert_eq!(block_ht - 1, parent_ht);
        Ok((parent, parent_ht))
    }

    /// Add a genesis block to the database.
    pub fn add_genesis(
        &mut self,
        block: StacksBlockId,
        put_list: Vec<PutCommand>,
    ) -> Result<(), SoarError> {
        if !self.storage.is_empty()? {
            return Err(SoarError::GenesisRewriteAttempted);
        }

        self.storage
            .store_genesis_block(block.clone(), put_list.clone())?;
        for put in put_list.into_iter() {
            self.storage.apply_put(put);
        }

        self.storage.set_current_block(block);

        Ok(())
    }

    /// Add a new block to the database, retargeting the database to
    ///  to the new block's fork if necessary.
    pub fn add_block_ops(
        &mut self,
        block: StacksBlockId,
        parent: StacksBlockId,
        put_list: Vec<PutCommand>,
    ) -> Result<(), SoarError> {
        // if needed, target the DB at the block's parent
        self.set_block(&parent)?;

        // then store and apply the block
        self.storage
            .store_block_data(block.clone(), parent, put_list.clone())?;
        for put in put_list.into_iter() {
            self.storage.apply_put(put);
        }
        self.storage.set_current_block(block);
        Ok(())
    }

    /// Add a new unconfirmed block to the database, retargeting the database to
    ///  to the new block's fork if necessary.
    pub fn add_unconfirmed_block_ops(
        &mut self,
        block: StacksBlockId,
        parent: StacksBlockId,
        put_list: Vec<PutCommand>,
    ) -> Result<(), SoarError> {
        // if needed, target the DB at the block's parent
        self.set_block(&parent)?;

        // then store and apply the block
        self.storage
            .store_unconfirmed_data(block, parent, put_list.clone())?;

        // do not set the unconfirmed state as the current block

        Ok(())
    }

    /// A lot of code in the stacks-blockchain codebase assumes
    ///  that a "sentinel block hash" indicates a non-existent parent
    ///  rather than using an Option<> type for the parent. This method inserts
    ///  a sentinel genesis block to handle that behavior
    pub fn stub_genesis(&mut self) {
        self.add_genesis(StacksBlockId([255; 32]), vec![]).unwrap();
    }

    /// Drop unconfirmed block state built off of confirmed StacksBlockId `block`
    pub fn drop_unconfirmed(&mut self, block: &StacksBlockId) {
        let unconfirmed_id = make_unconfirmed_chain_tip(block);
        self.storage.drop_unconfirmed(&unconfirmed_id)
    }

    pub fn begin<'a>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
    ) -> Result<PendingSoarBlock<'a>, SoarError> {
        if self.current_block() != Some(current) {
            Err(SoarError::ViewChangeRequired)
        } else {
            Ok(PendingSoarBlock {
                db: self,
                parent_block: current.clone(),
                id: next.clone(),
                pending_ops: vec![],
                pending_view: HashMap::new(),
                is_unconfirmed: false,
            })
        }
    }

    pub fn begin_read_only<'a>(
        &'a self,
        current: Option<&StacksBlockId>,
    ) -> Result<ReadOnlySoarConn<'a>, SoarError> {
        if let Some(current) = current {
            if self.current_block() != Some(current) {
                return Err(SoarError::ViewChangeRequired);
            }
        }
        Ok(ReadOnlySoarConn { db: self })
    }

    pub fn begin_unconfirmed<'a>(
        &'a mut self,
        current: &StacksBlockId,
    ) -> Result<PendingSoarBlock<'a>, SoarError> {
        if self.current_block() != Some(current) {
            Err(SoarError::ViewChangeRequired)
        } else {
            let next = make_unconfirmed_chain_tip(current);
            // is there existing unconfirmed state? if so, use it
            let (pending_ops, pending_view) = match self.storage.get_unconfirmed_state(&next)? {
                Some(x) => x,
                None => (vec![], HashMap::new()),
            };

            Ok(PendingSoarBlock {
                db: self,
                parent_block: current.clone(),
                id: next,
                pending_ops,
                pending_view,
                is_unconfirmed: true,
            })
        }
    }
}

/// Make an unconfirmed chain tip from an existing chain tip, so that it won't conflict with
/// the "true" chain tip after the state it represents is later reprocessed and confirmed.
fn make_unconfirmed_chain_tip(chain_tip: &StacksBlockId) -> StacksBlockId {
    let mut bytes = [0u8; 64];
    bytes[0..32].copy_from_slice(chain_tip.as_bytes());
    bytes[32..64].copy_from_slice(chain_tip.as_bytes());

    let h = Sha512Trunc256Sum::from_data(&bytes);
    let mut res_bytes = [0u8; 32];
    res_bytes[0..32].copy_from_slice(h.as_bytes());

    StacksBlockId(res_bytes)
}

impl<'a> PendingSoarBlock<'a> {
    pub fn as_clarity_db<'b>(
        &'b mut self,
        headers_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityDatabase<'b> {
        ClarityDatabase::new(self, headers_db, burn_state_db)
    }

    pub fn rollback_block(self) {
        // nothing needs to be done, just destroy the struct
    }

    pub fn commit_to(self, final_bhh: &StacksBlockId) {
        self.db
            .add_block_ops(final_bhh.clone(), self.parent_block, self.pending_ops)
            .expect("FAIL: error committing block to SoarDB");
    }

    pub fn test_commit(self) {
        let bhh = self.id.clone();
        self.commit_to(&bhh);
    }

    pub fn commit_unconfirmed(self) {
        self.db
            .add_unconfirmed_block_ops(self.id.clone(), self.parent_block, self.pending_ops)
            .expect("FAIL: error committing block to SoarDB");
    }

    // This is used by miners
    //   so that the block validation and processing logic doesn't
    //   reprocess the same data as if it were already loaded
    pub fn commit_mined_block(self, _will_move_to: &StacksBlockId) {
        // just destroy the struct and dropped the mined data
    }
}

fn metadata_key(contract: &QualifiedContractIdentifier, key: &str) -> String {
    format!("clr-soar-meta::{}::{}", contract, key)
}

impl<'a> ClarityBackingStore for ReadOnlySoarConn<'a> {
    fn put_all(&mut self, _items: Vec<(String, String)>) {
        panic!("BUG: attempted commit to read-only connection");
    }

    fn get(&mut self, key: &str) -> Option<String> {
        self.db
            .get_value(key)
            .expect("FAIL: Unhandled SoarDB error")
    }

    fn get_with_proof(&mut self, key: &str) -> Option<(String, Vec<u8>)> {
        match self.get(key) {
            Some(value) => Some((value, vec![])),
            None => None,
        }
    }

    fn set_block_hash(
        &mut self,
        _bhh: StacksBlockId,
    ) -> clarity::vm::errors::InterpreterResult<StacksBlockId> {
        panic!("SoarDB does not support set_block_hash");
    }

    fn get_block_at_height(&mut self, _height: u32) -> Option<StacksBlockId> {
        panic!("SoarDB does not support get_block_at_height");
    }

    fn get_current_block_height(&mut self) -> u32 {
        self.get_open_chain_tip_height()
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.db
            .get_block_height(&self.get_open_chain_tip())
            .expect("Failed to get block height of open chain tip")
            .try_into()
            .expect("Block height overflowed u32")
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.db
            .current_block()
            .cloned()
            .expect("SoarDB connection opened on empty db. Should run stub_genesis()")
    }

    /// SoarDB does *not* use a side store. Data is stored directly in the KV storage
    fn get_side_store(&mut self) -> &rusqlite::Connection {
        panic!("SoarDB does not implement a side-store");
    }

    fn get_cc_special_cases_handler(&self) -> Option<clarity::vm::database::SpecialCaseHandler> {
        None
    }

    fn insert_metadata(
        &mut self,
        _contract: &QualifiedContractIdentifier,
        _key: &str,
        _value: &str,
    ) {
        // NOOP
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> InterpreterResult<Option<String>> {
        let key = metadata_key(contract, key);
        Ok(self.get(&key))
    }

    fn get_metadata_manual(
        &mut self,
        _at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> InterpreterResult<Option<String>> {
        self.get_metadata(contract, key)
    }

    /// This method doesn't need to be used in the SoarDB, because metadata is stored quite differently
    ///  than in the MARF (which is what this method was used for). So, panic
    fn get_contract_hash(
        &mut self,
        _contract: &QualifiedContractIdentifier,
    ) -> InterpreterResult<(StacksBlockId, clarity::util::hash::Sha512Trunc256Sum)> {
        panic!("get_contract_hash() is not implemented in the SoarDB");
    }
}

impl<'a> ClarityBackingStore for PendingSoarBlock<'a> {
    fn put_all(&mut self, items: Vec<(String, String)>) {
        for (key, value) in items {
            let prior_value = self.get(&key);
            self.pending_view.insert(key.clone(), value.clone());
            let op = PutCommand {
                key,
                prior_value,
                value,
            };
            self.pending_ops.push(op);
        }
    }

    fn get(&mut self, key: &str) -> Option<String> {
        self.pending_view.get(key).cloned().or_else(|| {
            self.db
                .get_value(key)
                .expect("FAIL: Unhandled SoarDB error")
        })
    }

    fn get_with_proof(&mut self, key: &str) -> Option<(String, Vec<u8>)> {
        match self.get(key) {
            Some(value) => Some((value, vec![])),
            None => None,
        }
    }

    fn set_block_hash(
        &mut self,
        _bhh: StacksBlockId,
    ) -> clarity::vm::errors::InterpreterResult<StacksBlockId> {
        panic!("SoarDB does not support set_block_hash");
    }

    fn get_block_at_height(&mut self, _height: u32) -> Option<StacksBlockId> {
        panic!("SoarDB does not support get_block_at_height");
    }

    fn get_current_block_height(&mut self) -> u32 {
        self.get_open_chain_tip_height()
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.db
            .get_block_height(&self.parent_block)
            .expect("Failed to get block height of open parent")
            .checked_add(1)
            .expect("Overflowed u64 getting height")
            .try_into()
            .expect("Overflowed u32 getting height")
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.id.clone()
    }

    /// SoarDB does *not* use a side store. Data is stored directly in the KV storage
    fn get_side_store(&mut self) -> &rusqlite::Connection {
        panic!("SoarDB does not implement a side-store");
    }

    fn get_cc_special_cases_handler(&self) -> Option<clarity::vm::database::SpecialCaseHandler> {
        None
    }

    /// Metadata in SoarDB is handled very differently than the MARF-KV. The metadata in the MARF-KV
    ///  is kept intentionally separate from the MARF because the metadata shouldn't be included in the
    ///  data hash. SoarDB, however, does not perform data hashes, so the metadata can be included like
    ///  any other put operations.
    fn insert_metadata(&mut self, contract: &QualifiedContractIdentifier, key: &str, value: &str) {
        let key = metadata_key(contract, key);
        self.put_all(vec![(key, value.to_string())]);
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> InterpreterResult<Option<String>> {
        let key = metadata_key(contract, key);
        let r = self.get(&key);
        Ok(r)
    }

    fn get_metadata_manual(
        &mut self,
        _at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> InterpreterResult<Option<String>> {
        self.get_metadata(contract, key)
    }

    /// This method doesn't need to be used in the SoarDB, because metadata is stored quite differently
    ///  than in the MARF (which is what this method was used for). So, panic
    fn get_contract_hash(
        &mut self,
        _contract: &QualifiedContractIdentifier,
    ) -> InterpreterResult<(StacksBlockId, clarity::util::hash::Sha512Trunc256Sum)> {
        panic!("get_contract_hash() is not implemented in the SoarDB");
    }
}
