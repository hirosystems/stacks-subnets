//! In-memory backing storage option for SoarDB. This backing store
//! is transient, so any data stored in this will be lost when the
//! process exits.

use std::collections::HashMap;

use crate::SoarError;

use super::PutCommand;
use stacks_common::{info, types::chainstate::StacksBlockId};

pub struct BlockData {
    put_log: Vec<PutCommand>,
    parent: Option<StacksBlockId>,
    height: u64,
    #[allow(dead_code)]
    id: StacksBlockId,
}

pub struct UnconfirmedState {
    put_log: Vec<PutCommand>,
    parent: StacksBlockId,
    height: u64,
    #[allow(dead_code)]
    id: StacksBlockId,
    entries: HashMap<String, String>,
}

struct CurrentBlockMeta {
    id: StacksBlockId,
    unconfirmed: bool,
}

pub struct MemoryBackingStore {
    current_block: Option<StacksBlockId>,
    pub entries: HashMap<String, String>,
    blocks: HashMap<StacksBlockId, BlockData>,
    blocks_by_height: HashMap<u64, StacksBlockId>,
    pub unconfirmed_blocks: HashMap<StacksBlockId, UnconfirmedState>,
}

impl MemoryBackingStore {
    pub fn new() -> Self {
        MemoryBackingStore {
            current_block: None,
            entries: HashMap::new(),
            blocks: HashMap::new(),
            blocks_by_height: HashMap::new(),
            unconfirmed_blocks: HashMap::new(),
        }
    }

    /// Destroy this block's unconfirmed state if it exists, otherwise panic!
    pub fn drop_unconfirmed(&mut self, block: &StacksBlockId) {
        if self.current_block.as_ref() == Some(block) {
            self.undo_block(block)
                .expect("FATAL: failed to undo current block");
        }
        info!("Rolled back unconfirmed block"; "stacks_block_id" => %block);
        if self.unconfirmed_blocks.remove_entry(block).is_none() {
            panic!("FATAL: attempted unconfirmed rollback on either unknown block or a confirmed block")
        }
    }

    pub fn has_block(&self, block: &StacksBlockId) -> bool {
        self.blocks.contains_key(block) || self.unconfirmed_blocks.contains_key(block)
    }

    pub fn reapply_block(&mut self, block: &StacksBlockId) -> Result<(), SoarError> {
        let block_data = self
            .blocks
            .get(block)
            .ok_or_else(|| SoarError::BlockNotFound(block.clone()))?;

        let block_height = block_data.height;
        for command in block_data.put_log.clone().into_iter() {
            self.apply_put(command);
        }

        self.blocks_by_height.insert(block_height, block.clone());
        self.set_current_block(block.clone());

        Ok(())
    }

    pub fn get_unconfirmed_state(
        &self,
        block: &StacksBlockId,
    ) -> Result<Option<(Vec<PutCommand>, HashMap<String, String>)>, SoarError> {
        Ok(self
            .unconfirmed_blocks
            .get(block)
            .map(|state| (state.put_log.clone(), state.entries.clone())))
    }

    pub fn undo_block(&mut self, expected_cur_block: &StacksBlockId) -> Result<(), SoarError> {
        if self.current_block.is_none() || self.current_block.as_ref() != Some(expected_cur_block) {
            return Err(SoarError::MismatchViewDuringRollback);
        }

        let block_data = self
            .blocks
            .get(expected_cur_block)
            .expect("Could not find block data for current block");
        let parent = block_data.parent.clone();
        let block_height = block_data.height;

        // undo each operation in reverse order from the edit log
        for put_command in block_data.put_log.clone().into_iter().rev() {
            self.undo_put(put_command);
        }

        let block_id = self.blocks_by_height.remove(&block_height);
        assert_eq!(block_id.as_ref(), Some(expected_cur_block));

        // operations are undone, now set the current_block to the parent
        self.current_block = parent;

        Ok(())
    }

    pub fn get_value(&self, key: &str) -> Result<Option<String>, SoarError> {
        let current_block = match self.current_block.as_ref() {
            Some(x) => x,
            None => return Ok(None),
        };

        Ok(self.entries.get(key).cloned())
    }

    pub fn get_block_parent(&self, block: &StacksBlockId) -> Result<StacksBlockId, SoarError> {
        match self.blocks.get(&block) {
            Some(data) => match data.parent.as_ref() {
                Some(parent) => Ok(parent.clone()),
                None => Err(SoarError::NoParentBlock("No parent at zero-block")),
            },
            None => Err(SoarError::BlockNotFound(block.clone())),
        }
    }

    pub fn get_block_at_height(&self, height: u64) -> Result<Option<StacksBlockId>, SoarError> {
        Ok(self.blocks_by_height.get(&height).cloned())
    }

    pub fn get_block_height(&self, block: &StacksBlockId) -> Result<u64, SoarError> {
        match self.blocks.get(&block) {
            Some(data) => Ok(data.height),
            None => Err(SoarError::BlockNotFound(block.clone())),
        }
    }

    pub fn set_current_block(&mut self, block: StacksBlockId) {
        self.current_block = Some(block);
    }

    pub fn current_block(&self) -> Option<&StacksBlockId> {
        self.current_block.as_ref()
    }

    pub fn is_empty(&self) -> Result<bool, SoarError> {
        Ok(self.current_block.is_none() && self.blocks.is_empty() && self.entries.is_empty())
    }

    pub fn store_genesis_block(
        &mut self,
        block: StacksBlockId,
        put_log: Vec<PutCommand>,
    ) -> Result<(), SoarError> {
        if self.current_block.is_some() {
            return Err(SoarError::GenesisRewriteAttempted);
        }

        self.blocks_by_height.insert(0, block.clone());

        let prior = self.blocks.insert(
            block.clone(),
            BlockData {
                id: block,
                parent: None,
                put_log,
                height: 0,
            },
        );

        assert!(
            prior.is_none(),
            "Stored block data over an existing block entry"
        );

        Ok(())
    }

    /// Store a new unconfirmed block, and apply all of its puts
    ///  to the unconfirmed block data
    pub fn store_unconfirmed_data(
        &mut self,
        block: StacksBlockId,
        parent: StacksBlockId,
        put_log: Vec<PutCommand>,
    ) -> Result<(), SoarError> {
        let parent_height = match self.blocks.get(&parent) {
            Some(parent_data) => Ok(parent_data.height),
            None => Err(SoarError::NoParentBlock(
                "Parent block has not been processed yet",
            )),
        }?;

        let entries = put_log
            .iter()
            .map(|PutCommand { key, value, .. }| (key.clone(), value.clone()))
            .collect();

        let prior = self.unconfirmed_blocks.insert(
            block.clone(),
            UnconfirmedState {
                put_log,
                parent,
                height: parent_height,
                id: block,
                entries,
            },
        );

        assert!(
            prior.is_none(),
            "Stored unconfirmed block data over an existing block entry"
        );

        Ok(())
    }

    pub fn store_block_data(
        &mut self,
        block: StacksBlockId,
        parent: StacksBlockId,
        put_log: Vec<PutCommand>,
    ) -> Result<(), SoarError> {
        let parent_height = match self.blocks.get(&parent) {
            Some(parent_data) => Ok(parent_data.height),
            None => Err(SoarError::NoParentBlock(
                "Parent block has not been processed yet",
            )),
        }?;

        let height = parent_height
            .checked_add(1)
            .ok_or_else(|| SoarError::BlockHeightOverflow)?;
        self.blocks_by_height.insert(height, block.clone());

        let prior = self.blocks.insert(
            block.clone(),
            BlockData {
                id: block,
                parent: Some(parent),
                put_log,
                height,
            },
        );
        assert!(
            prior.is_none(),
            "Stored block data over an existing block entry"
        );
        Ok(())
    }

    pub fn apply_put(&mut self, command: PutCommand) {
        self.entries.insert(command.key, command.value);
    }

    pub fn undo_put(&mut self, command: PutCommand) {
        let old_value = if let Some(old_value) = command.prior_value {
            self.entries.insert(command.key, old_value)
        } else {
            self.entries.remove(&command.key)
        };
        assert_eq!(
            old_value,
            Some(command.value),
            "Undo operation applied to an entry that had an unexpected value"
        );
    }
}
