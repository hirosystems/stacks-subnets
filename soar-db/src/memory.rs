use std::collections::HashMap;

use crate::SoarError;

use super::PutCommand;
use stacks_common::types::chainstate::StacksBlockId;

pub struct BlockData {
    put_log: Vec<PutCommand>,
    parent: Option<StacksBlockId>,
    height: u64,
    #[allow(dead_code)]
    id: StacksBlockId,
}

pub struct MemoryBackingStore {
    current_block: Option<StacksBlockId>,
    pub entries: HashMap<String, String>,
    blocks: HashMap<StacksBlockId, BlockData>,
    blocks_by_height: HashMap<u64, StacksBlockId>,
}

impl MemoryBackingStore {
    pub fn new() -> Self {
        MemoryBackingStore {
            current_block: None,
            entries: HashMap::new(),
            blocks: HashMap::new(),
            blocks_by_height: HashMap::new(),
        }
    }

    pub fn has_block(&self, block: &StacksBlockId) -> bool {
        self.blocks.contains_key(block)
    }

    pub fn reapply_block(&mut self, block: &StacksBlockId) -> Result<(), SoarError> {
        let block_data = self
            .blocks
            .get(block)
            .ok_or_else(|| SoarError::BlockNotFound(block.clone()))?;

        for command in block_data.put_log.clone().into_iter() {
            self.apply_put(command);
        }

        self.set_current_block(block.clone());

        Ok(())
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

        // undo each operation in reverse order from the edit log
        for put_command in block_data.put_log.clone().into_iter().rev() {
            self.undo_put(put_command);
        }

        // operations are undone, now set the current_block to the parent
        self.current_block = parent;

        Ok(())
    }

    pub fn get_value(&self, key: &str) -> Result<Option<String>, SoarError> {
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

        let prior = self.blocks.insert(
            block.clone(),
            BlockData {
                id: block,
                parent: Some(parent),
                put_log,
                height: parent_height
                    .checked_add(1)
                    .ok_or_else(|| SoarError::BlockHeightOverflow)?,
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
