// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use clarity::vm::costs::cost_functions::ClarityCostFunction;
use clarity::vm::costs::{CostTracker, MemoryConsumer};
use std::cmp;
use std::convert::{TryFrom, TryInto};

use clarity::vm::contexts::{Environment, GlobalContext};
use clarity::vm::errors::Error;
use clarity::vm::errors::{
    CheckErrors, InterpreterError, InterpreterResult as Result, RuntimeErrorType,
};
use clarity::vm::representations::{ClarityName, SymbolicExpression, SymbolicExpressionType};
use clarity::vm::types::{
    BuffData, PrincipalData, QualifiedContractIdentifier, SequenceData, TupleData, TypeSignature,
    Value,
};

use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::StacksMicroblockHeader;
use crate::util_lib::boot::boot_code_id;

use clarity::vm::events::{STXEventType, STXLockEventData, StacksTransactionEvent};

use stacks_common::util::hash::Hash160;

use crate::vm::costs::runtime_cost;

/// Handle special cases of contract-calls -- namely, those into PoX that should lock up STX
pub fn handle_contract_call_special_cases(
    global_context: &mut GlobalContext,
    sender: Option<&PrincipalData>,
    _sponsor: Option<&PrincipalData>,
    contract_id: &QualifiedContractIdentifier,
    function_name: &str,
    args: &[Value],
    result: &Value,
) -> Result<()> {
    Ok(())
}
