use crate::burnchains::{Burnchain, StacksSubnetOp, StacksSubnetOpType};
use crate::chainstate::burn::db::sortdb::SortitionHandleTx;
use crate::chainstate::burn::operations::DepositNftOp;
use crate::chainstate::burn::operations::Error as op_error;
use clarity::types::chainstate::BurnchainHeaderHash;
use std::convert::TryFrom;

impl TryFrom<&StacksSubnetOp> for DepositNftOp {
    type Error = op_error;

    fn try_from(value: &StacksSubnetOp) -> Result<Self, Self::Error> {
        if let StacksSubnetOpType::DepositNft {
            ref l1_contract_id,
            ref subnet_contract_id,
            ref id,
            ref sender,
        } = value.event
        {
            Ok(DepositNftOp {
                txid: value.txid.clone(),
                // use the StacksBlockId in the L1 event as the burnchain header hash
                burn_header_hash: BurnchainHeaderHash(value.in_block.0.clone()),
                l1_contract_id: l1_contract_id.clone(),
                subnet_contract_id: subnet_contract_id.clone(),
                id: id.clone(),
                sender: sender.clone(),
            })
        } else {
            Err(op_error::InvalidInput)
        }
    }
}

impl DepositNftOp {
    pub fn check(
        &self,
        _burnchain: &Burnchain,
        _tx: &mut SortitionHandleTx,
    ) -> Result<(), op_error> {
        // good to go!
        Ok(())
    }

    #[cfg(test)]
    pub fn set_burn_height(&mut self, _height: u64) {}
}
