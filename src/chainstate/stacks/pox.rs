/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::io;
use std::io::prelude::*;
use std::io::{Read, Write, Seek, SeekFrom};
use std::fmt;
use std::fs;
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::convert::From;

use rusqlite::Connection;
use rusqlite::DatabaseName;

use core::*;

use burnchains::bitcoin::address::BitcoinAddress;
use burnchains::Address;

use chainstate::burn::operations::*;

use chainstate::stacks::Error;
use chainstate::stacks::db::accounts::MinerReward;
use chainstate::stacks::*;
use chainstate::stacks::db::*;
use chainstate::stacks::db::transactions::TransactionNonceMismatch;

use chainstate::burn::BlockSnapshot;

use std::path::{Path, PathBuf};

use util::db::Error as db_error;
use util::db::{
    DBConn,
    FromRow,
    FromColumn,
    query_row,
    query_rows,
    query_row_columns,
    query_count,
    query_int,
    tx_busy_handler,
};

use util::strings::StacksString;
use util::get_epoch_time_secs;
use util::hash::to_hex;
use util::db::u64_to_sql;

use util::retry::BoundReader;

use chainstate::burn::db::burndb::*;

use net::MAX_MESSAGE_LEN;
use net::BLOCKS_INV_DATA_MAX_BITLEN;
use net::BlocksInvData;
use net::Error as net_error;

use vm::types::{
    Value,
    AssetIdentifier,
    TupleData,
    PrincipalData,
    StandardPrincipalData,
    QualifiedContractIdentifier,
    TypeSignature
};

use vm::contexts::{
    AssetMap
};

use vm::ast::build_ast;
use vm::analysis::run_analysis;

use vm::clarity::{
    ClarityBlockConnection,
    ClarityConnection,
    ClarityInstance
};

pub use vm::analysis::errors::{CheckErrors, CheckError};

use vm::database::ClarityDatabase;

use vm::contracts::Contract;

use rand::RngCore;
use rand::thread_rng;

use rusqlite::{
    Error as sqlite_error,
    OptionalExtension
};

pub const POX_PARTICIPATION_LOWER_LIMIT_DENOM: u128 = 4;

pub struct PoxLockLogEntry {
    pub locked_address: StacksAddress,
    pub until_burn_height: u64,
    pub amount: u128,
    pub reward_address: BitcoinAddress,
}

/// A log of all proof-of-transfer locks which
///  _may_ be valid at a given reward period. When
///  processing a PoX-Anchor block, this log should be
///  stale-lock collected.
pub struct PoxLockLog {
    locks: Vec<PoxLockLogEntry>
}


impl PoxLockLog {
    pub fn collect_stale(&mut self, reward_end_height: u64) {
        self.locks.retain(|entry| entry.until_burn_height >= reward_end_height)
    }

    pub fn locked_amount(&self) -> u128 {
        self.locks.iter().fold(0, |acc, entry| {
            acc.checked_add(entry.amount)
                .expect("OVERFLOW: 2^128 or greater POX lockup amount")
        })
    }

    pub fn reward_set(&self, stx_liquid_supply: u128) -> Vec<BitcoinAddress> {
        let locked_stx = self.locked_amount();
        let participation_numer = cmp::min(stx_liquid_supply / POX_PARTICIPATION_LOWER_LIMIT_DENOM,
                                           locked_stx);
        let participation_threshold = participation_numer / 5000;
        let participation_threshold =
            if participation_threshold % 10000 == 0 {
                participation_threshold
            } else {
                participation_threshold + (10000 - participation_threshold % 10000)
            };

        let mut reward_set = vec![];
        for entry in self.locks.iter() {
            let reward_count = entry.amount / participation_threshold;
            for _i in 0..reward_count {
                reward_set.push(entry.reward_address.clone())
            }
        }
        reward_set
    }

    pub fn add_lock(&mut self, locked_address: StacksAddress, until_burn_height: u64, amount: u128, reward_address: BitcoinAddress) {
        self.locks.push(PoxLockLogEntry {
            locked_address,
            until_burn_height,
            amount,
            reward_address })
    }
}
