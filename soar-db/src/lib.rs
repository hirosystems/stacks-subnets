// (S)ubnets (O)ptimistic (A)daptive (R)eplay DB
extern crate clarity;
extern crate stacks_common;

use std::collections::HashMap;

use crate::memory::MemoryBackingStore;
use clarity::vm::database::ClarityBackingStore;
use stacks_common::types::chainstate::StacksBlockId;

pub mod memory;

#[cfg(test)]
pub mod tests;

pub trait SoarBackingStore {}

/// Key-Value Store with edit log
pub struct SoarDB {
    storage: MemoryBackingStore,
}

#[derive(Clone)]
pub struct PutCommand {
    key: String,
    /// If a prior value existed for this entry, store it here
    /// If this is the first value for the key, this will be None
    prior_value: Option<String>,
    value: String,
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

    /// Fetch the parent of `block` and its block height, checking that it matches `block_ht - 1`
    fn get_block_parent(
        &self,
        block: &StacksBlockId,
        block_ht: u64,
    ) -> Result<(StacksBlockId, u64), String> {
        if block_ht == 0 {
            return Err("No parent block for the zero-height block".into());
        }
        let parent = self.storage.get_block_parent(&block)?;
        let parent_ht = self.storage.get_block_height(&parent)?;
        assert_eq!(block_ht - 1, parent_ht);
        Ok((parent, parent_ht))
    }

    pub fn add_genesis(
        &mut self,
        block: StacksBlockId,
        put_list: Vec<PutCommand>,
    ) -> Result<(), String> {
        if !self.storage.is_empty()? {
            return Err("Attempted to add genesis block to DB that already has data".into());
        }

        self.storage.store_genesis_block(block.clone(), put_list);
        for put in put_list.into_iter() {
            self.storage.apply_put(put);
        }

        self.storage.set_current_block(block);

        Ok(())
    }

    pub fn add_block_ops(
        &mut self,
        block: StacksBlockId,
        parent: StacksBlockId,
        put_list: Vec<PutCommand>,
    ) -> Result<(), String> {
        if self.current_block().is_none() || self.current_block().unwrap() == &parent {
            // this block is an extension
            self.storage
                .store_block_data(block.clone(), parent, put_list.clone())?;

            for put in put_list.into_iter() {
                self.storage.apply_put(put);
            }

            self.storage.set_current_block(block);
        } else {
            // this block is not an extension and requires a rollback!
            // Step 1: find the "fork point", which is the most recent common ancestor
            //         of `block` and `current_block()`
            //
            //  We will do this by using the *block height* to walk backwards until the
            //   two ancestor paths meet. First, we find ancestors at the same height,
            //   then we loop until the ancestors are equal.
            if !self.storage.has_block(&parent) {
                return Err("Parent block has not been processed yet".into());
            }

            // unwrap is safe, because current_block().is_none() is checked in branch above
            let mut ancestor_a = self.current_block().unwrap().clone();
            let mut ancestor_b = parent.clone();
            let mut ancestor_a_ht = self.storage.get_block_height(&ancestor_a)?;
            let mut ancestor_b_ht = self.storage.get_block_height(&ancestor_b)?;
            // we track the path of "ancestor b" so that we know what needs to be
            //  applied to get from the `fork_point` to `parent`
            let mut ancestors_b = vec![parent.clone()];

            while ancestor_a_ht != ancestor_b_ht {
                if ancestor_a_ht > ancestor_b_ht {
                    (ancestor_a, ancestor_a_ht) =
                        self.get_block_parent(&ancestor_a, ancestor_a_ht)?;
                } else {
                    (ancestor_b, ancestor_b_ht) =
                        self.get_block_parent(&ancestor_b, ancestor_b_ht)?;
                    ancestors_b.push(ancestor_b.clone());
                }
            }

            while ancestor_a != ancestor_b {
                (ancestor_a, ancestor_a_ht) = self.get_block_parent(&ancestor_a, ancestor_a_ht)?;
                (ancestor_b, ancestor_b_ht) = self.get_block_parent(&ancestor_b, ancestor_b_ht)?;
                ancestors_b.push(ancestor_b.clone());
            }

            // Now, we have the most recent common ancestor (ancestor_a == ancestor_b)
            // We can now execute Step 2: undo from the current block to the common ancestor

            let fork_point = ancestor_a;
            let mut current_block = self
                .current_block()
                .ok_or_else(|| "Tried to rollback from genesis")?
                .clone();
            while &current_block != &fork_point {
                self.storage.undo_block(&current_block)?;
                current_block = self
                    .current_block()
                    .ok_or_else(|| "Tried to rollback beyond genesis")?
                    .clone();
            }

            // Step 3: apply all the blocks from `fork_point` through to `parent`, and then
            //  apply the new block
            for block_to_apply in ancestors_b.iter().rev() {
                current_block = self
                    .current_block()
                    .ok_or_else(|| "Tried to rollback beyond genesis")?
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
                .ok_or_else(|| "Tried to rollback beyond genesis")?
                .clone();
            assert_eq!(
                current_block, parent,
                "Failed while replaying operations: expected parent and current block to align"
            );

            // Finally, we can apply the new block
            self.storage
                .store_block_data(block.clone(), parent, put_list.clone())?;
            for put in put_list.into_iter() {
                self.storage.apply_put(put);
            }
            self.storage.set_current_block(block);
        }
        Ok(())
    }
}
