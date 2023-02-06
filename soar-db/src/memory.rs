use std::collections::HashMap;

use super::PutCommand;
use stacks_common::types::chainstate::StacksBlockId;

pub struct BlockData {
    put_log: Vec<PutCommand>,
    parent: Option<StacksBlockId>,
    height: u64,
    id: StacksBlockId,
}

pub struct MemoryBackingStore {
    current_block: Option<StacksBlockId>,
    entries: HashMap<String, String>,
    blocks: HashMap<StacksBlockId, BlockData>,
}

impl MemoryBackingStore {
    pub fn new() -> Self {
        MemoryBackingStore {
            current_block: None,
            entries: HashMap::new(),
            blocks: HashMap::new(),
        }
    }

    pub fn has_block(&self, block: &StacksBlockId) -> bool {
        self.blocks.contains_key(block)
    }

    pub fn reapply_block(&mut self, block: &StacksBlockId) -> Result<(), String> {
        let block_data = self.blocks.get(block).ok_or_else(|| "No such block")?;

        for command in block_data.put_log.clone().into_iter() {
            self.apply_put(command);
        }

        Ok(())
    }

    pub fn undo_block(&mut self, expected_cur_block: &StacksBlockId) -> Result<(), String> {
        if self.current_block.is_none() || self.current_block.as_ref() != Some(expected_cur_block) {
            return Err("Expected current block does not match storage's view of current block during rollback".into());
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

    pub fn get_block_parent(&self, block: &StacksBlockId) -> Result<StacksBlockId, String> {
        match self.blocks.get(&block) {
            Some(data) => match data.parent.as_ref() {
                Some(parent) => Ok(parent.clone()),
                None => Err("Block is zero-height and has no parent".into()),
            },
            None => Err(format!("{} not found in storage", block)),
        }
    }

    pub fn get_block_height(&self, block: &StacksBlockId) -> Result<u64, String> {
        match self.blocks.get(&block) {
            Some(data) => Ok(data.height),
            None => Err(format!("{} not found in storage", block)),
        }
    }

    pub fn set_current_block(&mut self, block: StacksBlockId) {
        self.current_block = Some(block);
    }

    pub fn current_block(&self) -> Option<&StacksBlockId> {
        self.current_block.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.current_block.is_none() && self.blocks.is_empty() && self.entries.is_empty()
    }

    pub fn store_genesis_block(
        &mut self,
        block: StacksBlockId,
        put_log: Vec<PutCommand>,
    ) -> Result<(), String> {
        if self.current_block.is_some() {
            return Err("Attempted to store genesis block in DB with existing data".into());
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
    ) -> Result<(), String> {
        let parent_height = match self.blocks.get(&parent) {
            Some(parent_data) => Ok(parent_data.height),
            None => Err("Parent block has not been processed yet"),
        }?;

        let prior = self.blocks.insert(
            block.clone(),
            BlockData {
                id: block,
                parent: Some(parent),
                put_log,
                height: parent_height
                    .checked_add(1)
                    .ok_or_else(|| "Block height overflowed u64")?,
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
