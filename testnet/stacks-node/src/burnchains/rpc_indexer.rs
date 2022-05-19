use std::cmp::{self, Ordering};
use std::sync::Arc;
use std::{fs, io};

use rusqlite::{params, Connection, OpenFlags, Row, ToSql, Transaction, NO_PARAMS};
use stacks::burnchains::events::NewBlock;
use stacks::net::ExtendedStacksHeader;
use stacks::util::hash::Sha512Trunc256Sum;
use stacks::vm::database::ClaritySerializable;
use stacks::vm::types::{QualifiedContractIdentifier, SequenceData, TupleData};

use super::mock_events::{BlockIPC, MockHeader};
use super::{BurnchainChannel, Error};
use clarity::vm::Value as ClarityValue;
use stacks::burnchains::indexer::BurnBlockIPC;
use stacks::burnchains::indexer::BurnchainBlockDownloader;
use stacks::burnchains::indexer::BurnchainIndexer;
use stacks::burnchains::indexer::{BurnHeaderIPC, BurnchainBlockParser};
use stacks::burnchains::{
    BurnchainBlock, Error as BurnchainError, StacksHyperBlock, StacksHyperOp, Txid,
};
use stacks::core::StacksEpoch;
use stacks::types::chainstate::{BurnchainHeaderHash, StacksBlockId};
use stacks::util_lib::db::Error as db_error;
use stacks::util_lib::db::{query_row, query_rows, u64_to_sql, FromRow};

pub struct RpcStacksIndexer {
    first_block_height: u64,
    first_block_header_hash: BurnchainHeaderHash,
    first_block_timestamp: u64,
    client: L1RpcClient,
    db: Connection,
    headers_path: String,
    l1_contract: QualifiedContractIdentifier,
}

pub struct RpcBlockParser {}

pub struct RpcBlockDownloader {
    client: L1RpcClient,
    l1_contract: QualifiedContractIdentifier,
}

pub struct L1RpcClient {
    l1_rpc_interface: String,
}

#[derive(Deserialize)]
struct RpcInfoResponse {
    stacks_tip_height: u64,
    stacks_tip: String,
}

#[derive(Deserialize)]
struct RpcReadOnlyResponse {
    okay: bool,
    result: String,
}

/// Iterates headers from newest to oldest
struct L1RpcHeaderIterator<'a> {
    /// list of headers from *oldest* to *newest*
    headers: Vec<ExtendedStacksHeader>,
    last_tip: Option<StacksBlockId>,
    client: &'a L1RpcClient,
}

impl<'a> L1RpcHeaderIterator<'a> {
    fn new(
        client: &'a L1RpcClient,
        fetch_count: u64,
        from_tip: Option<&StacksBlockId>,
    ) -> Result<L1RpcHeaderIterator<'a>, Error> {
        let headers = client.get_headers(fetch_count, from_tip)?;
        Ok(Self {
            headers,
            client,
            last_tip: None,
        })
    }
}

impl<'a> Iterator for L1RpcHeaderIterator<'a> {
    type Item = ExtendedStacksHeader;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(header) = self.headers.pop() {
            let index_hash = header.header.index_block_hash(&header.consensus_hash);
            self.last_tip = Some(index_hash);
            Some(header)
        } else if let Some(next_tip) = self.last_tip.take() {
            // need to query for the next set of headers
            match self.client.get_headers(u64::MAX, Some(&next_tip)) {
                Ok(mut headers) => {
                    headers.reverse();
                    self.headers = headers;
                    // recurse: check if the headers vec can pop(), set last_tip, etc.
                    self.next()
                }
                Err(e) => {
                    warn!("Error fetching next set of headers"; "err" => ?e);
                    None
                }
            }
        } else {
            // Finished iterating: the last set of headers was empty
            None
        }
    }
}

impl From<Error> for BurnchainError {
    fn from(e: Error) -> Self {
        BurnchainError::Bitcoin(e.to_string())
    }
}

impl L1RpcClient {
    pub fn invoke_contract(
        &self,
        contract: &QualifiedContractIdentifier,
        function_name: &str,
        arguments: &[ClarityValue],
    ) -> Result<ClarityValue, Error> {
        let url = format!(
            "{}/v2/contracts/call-read/{}/{}/{}",
            &self.l1_rpc_interface, &contract.issuer, &contract.name, function_name
        );

        let args_hex: Vec<_> = arguments.iter().map(|x| x.serialize()).collect();
        let body = serde_json::json!(
            { "sender": "SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF",
              "arguments": args_hex }
        );

        let client = reqwest::blocking::Client::new();
        let res = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()?;

        if res.status().is_success() {
            let res: RpcReadOnlyResponse = res.json()?;
            if res.okay {
                ClarityValue::try_deserialize_hex_untyped(&res.result)
                    .map_err(|e| Error::RPCError(e.to_string()))
            } else {
                warn!("Read-only invocation failure"; "result" => res.result);
                Err(Error::RPCError("Read-only invocation failed".into()))
            }
        } else {
            warn!("Read-only invocation failure"; "status_code" => res.status().to_string());
            Err(Error::RPCError("Read-only invocation failed".into()))
        }
    }

    pub fn get_stacks_block_height(&self) -> Result<u64, Error> {
        let url = format!("{}/v2/info", &self.l1_rpc_interface);
        let res: RpcInfoResponse = reqwest::blocking::get(url)?.json()?;
        Ok(res.stacks_tip_height)
    }

    /// Return one result set of headers from the L1 node in order from *newest* to *oldest*
    pub fn get_headers(
        &self,
        fetch_count: u64,
        from_tip: Option<&StacksBlockId>,
    ) -> Result<Vec<ExtendedStacksHeader>, Error> {
        let fetch_count = cmp::min(cmp::max(1, fetch_count), 2100);
        let tip_query_string = match from_tip {
            Some(x) => format!("?tip={}", x),
            None => "".into(),
        };

        let url = format!(
            "{}/v2/headers/{}{}",
            &self.l1_rpc_interface, fetch_count, tip_query_string,
        );
        let headers = reqwest::blocking::get(url)?.json()?;
        Ok(headers)
    }

    /// Walk all headers from the L1 node's chain tip
    fn walk_headers(
        &self,
        fetch_count: u64,
        from_tip: Option<&StacksBlockId>,
    ) -> Result<L1RpcHeaderIterator, BurnchainError> {
        L1RpcHeaderIterator::new(self, fetch_count, from_tip).map_err(BurnchainError::from)
    }
}

impl RpcStacksIndexer {
    pub fn store_block_header(
        conn: &Connection,
        header: ExtendedStacksHeader,
    ) -> Result<(), BurnchainError> {
        let height = header.header.total_work.work;
        let index_hash = header.header.index_block_hash(&header.consensus_hash);
        let parent_index_hash = &header.parent_block_id;
        let timestamp = header.header.total_work.burn;

        let params = params![
            u64_to_sql(height)?,
            &index_hash,
            parent_index_hash,
            u64_to_sql(timestamp)?,
        ];

        conn.execute(
            "INSERT INTO headers ( height, index_hash, parent_index_hash, timestamp ) VALUES (?, ?, ?, ?)",
            params,
        )?;

        Ok(())
    }

    pub fn sync_all_headers(&mut self) -> Result<(), BurnchainError> {
        let my_highest_header = self.get_highest_header()?;
        let first_block_height = self.get_first_block_height();

        let sql_tx = self.db.transaction()?;
        for header in self.client.walk_headers(u64::MAX, None)? {
            let header_height = header.header.total_work.work;
            if let Some(ref my_highest_header) = my_highest_header {
                if my_highest_header.height == header_height {
                    // synced to our highest header. check to make sure
                    //  that we haven't synced on a reorg!
                    let header_index_hash = header.header.index_block_hash(&header.consensus_hash);
                    if my_highest_header.index_hash != header_index_hash {
                        // yep, we synced on a reorg. drop the sql tx, and return a SyncAgain error
                        return Err(BurnchainError::TrySyncAgain);
                    } else {
                        // reached our own height and header hashes matched: finished syncing!
                        break;
                    }
                }
            }
            if header_height <= first_block_height {
                break;
            }
            Self::store_block_header(&sql_tx, header)?;
        }
        sql_tx.commit()?;

        Ok(())
    }

    pub fn get_highest_header(&self) -> Result<Option<MockHeader>, BurnchainError> {
        let query = "SELECT (height, index_hash, parent_index_hash, timestamp) FROM headers ORDER BY height DESC LIMIT 1";
        let result = query_row(&self.db, query, rusqlite::NO_PARAMS)?;
        Ok(result)
    }

    pub fn get_header_at(&self, height: u64) -> Result<Option<MockHeader>, BurnchainError> {
        let query = "SELECT (height, index_hash, parent_index_hash, timestamp) FROM headers WHERE height = ? LIMIT 1";
        let result = query_row(&self.db, query, params![u64_to_sql(height)?])?;
        Ok(result)
    }
}

impl FromRow<MockHeader> for MockHeader {
    fn from_row<'a>(row: &'a Row) -> Result<MockHeader, db_error> {
        let height: i64 = row.get("height")?;
        let index_hash = row.get("index_hash")?;
        let parent_index_hash = row.get("parent_index_hash")?;
        let time_stamp: i64 = row.get("timestamp")?;

        Ok(MockHeader {
            height: height as u64,
            index_hash,
            parent_index_hash,
            time_stamp: time_stamp as u64,
        })
    }
}

const DB_HEADERS_SCHEMAS: &'static [&'static str] = &[&r#"
    CREATE TABLE headers (
        height INTEGER NOT NULL,
        index_hash TEXT PRIMARY KEY NOT NULL,
        parent_index_hash TEXT NOT NULL,
        timestamp INTEGER NOT NULL,
    );
    "#];

impl BurnchainBlockParser for RpcBlockParser {
    type B = RpcBlockData;

    fn parse(&mut self, block: &RpcBlockData) -> Result<BurnchainBlock, BurnchainError> {
        Ok(BurnchainBlock::StacksHyperBlock(StacksHyperBlock {
            current_block: block.header.index_hash.clone(),
            parent_block: block.header.parent_index_hash.clone(),
            block_height: block.header.height,
            ops: block.operations.clone(),
        }))
    }
}

fn make_clarity_ascii(from: &str) -> ClarityValue {
    assert!(from.is_ascii());
    ClarityValue::string_ascii_from_bytes(from.bytes().collect())
        .expect("Failed to construct literal Clarity ascii string")
}

#[derive(Clone)]
pub struct RpcBlockData {
    operations: Vec<StacksHyperOp>,
    header: MockHeader,
}

impl BurnchainBlockDownloader for RpcBlockDownloader {
    type B = RpcBlockData;

    fn download(&mut self, header: &MockHeader) -> Result<Self::B, BurnchainError> {
        let block_value = self.client.invoke_contract(
            &self.l1_contract,
            "get-block-content",
            &[ClarityValue::UInt(header.height.into())],
        )?;

        // parse out the expected structure of the block contents
        // { block-commit: block-commit, ft-deposits: ft-deps, nft-deposits: nft-deps, stx-deposits: stx-deps }
        let mut tuple_data = if let ClarityValue::Tuple(data) = block_value {
            data.data_map
        } else {
            warn!("Bad type returned to downloader, expected tuple"; "found_type" => %block_value);
            return Err(
                Error::RPCError("Bad type returned to downloader, expected tuple.".into()).into(),
            );
        };

        let mut simulated_events = vec![];

        if let Some(ClarityValue::Optional(opt_data)) = tuple_data.remove("block-commit".into()) {
            // if a commit was made, append to the simulated events
            if let Some(commit) = opt_data.data {
                let event_tuple = TupleData::from_data(vec![
                    ("event".into(), make_clarity_ascii("block-commit")),
                    ("block-commit".into(), *commit),
                ])
                .expect("Failed to construct event tuple");

                simulated_events.push(ClarityValue::from(event_tuple));
            }
        } else {
            warn!("Bad tuple returned to downloader, expected block-commit key to be an optional");
            return Err(Error::RPCError(
                "Bad type returned to downloader, expected block-commit key.".into(),
            )
            .into());
        };

        if let Some(ClarityValue::Sequence(SequenceData::List(l))) =
            tuple_data.remove("ft-deposits".into())
        {
            for ft_deposit_tuple in l.data.into_iter() {
                if let ClarityValue::Tuple(mut t) = ft_deposit_tuple {
                    t.data_map
                        .insert("event".into(), make_clarity_ascii("deposit-ft"));
                    simulated_events.push(ClarityValue::from(t));
                } else {
                    warn!("Bad tuple returned to downloader, expected ft-deposits key to be a list of tuples");
                    return Err(Error::RPCError(
                        "Bad type returned to downloader, expected ft-deposits key.".into(),
                    )
                    .into());
                }
            }
        } else {
            warn!("Bad tuple returned to downloader, expected ft-deposits key to be a list");
            return Err(Error::RPCError(
                "Bad type returned to downloader, expected ft-deposits key.".into(),
            )
            .into());
        };

        if let Some(ClarityValue::Sequence(SequenceData::List(l))) =
            tuple_data.remove("nft-deposits".into())
        {
            for nft_deposit_tuple in l.data.into_iter() {
                if let ClarityValue::Tuple(mut t) = nft_deposit_tuple {
                    t.data_map
                        .insert("event".into(), make_clarity_ascii("deposit-nft"));
                    simulated_events.push(ClarityValue::from(t));
                } else {
                    warn!("Bad tuple returned to downloader, expected nft-deposits key to be a list of tuples");
                    return Err(Error::RPCError(
                        "Bad type returned to downloader, expected nft-deposits key.".into(),
                    )
                    .into());
                }
            }
        } else {
            warn!("Bad tuple returned to downloader, expected nft-deposits key to be a list");
            return Err(Error::RPCError(
                "Bad type returned to downloader, expected nft-deposits key.".into(),
            )
            .into());
        };

        if let Some(ClarityValue::Sequence(SequenceData::List(l))) =
            tuple_data.remove("stx-deposits".into())
        {
            for stx_deposit_tuple in l.data.into_iter() {
                if let ClarityValue::Tuple(mut t) = stx_deposit_tuple {
                    t.data_map
                        .insert("event".into(), make_clarity_ascii("deposit-stx"));
                    simulated_events.push(ClarityValue::from(t));
                } else {
                    warn!("Bad tuple returned to downloader, expected stx-deposits key to be a list of tuples");
                    return Err(Error::RPCError(
                        "Bad type returned to downloader, expected stx-deposits key.".into(),
                    )
                    .into());
                }
            }
        } else {
            warn!("Bad tuple returned to downloader, expected stx-deposits key to be a list");
            return Err(Error::RPCError(
                "Bad type returned to downloader, expected stx-deposits key.".into(),
            )
            .into());
        };

        // We reuse the HyperOps events parser to handle these.
        // However, we don't have txids for this data, so we have to use invented txids for these transactions
        let index_block_hash = &header.index_hash;

        let parsing_result: Result<Vec<_>, _> = simulated_events
            .into_iter()
            .enumerate()
            .map(|(event_index, event_tuple)| {
                let event_index =
                    u32::try_from(event_index).expect("More than u32::MAX events in a block");
                let mut data = index_block_hash.as_bytes().to_vec();
                data.extend_from_slice(&event_index.to_le_bytes());
                let invented_txid_data = Sha512Trunc256Sum::from_data(&data).to_bytes();

                StacksHyperOp::try_from_clar_value(
                    event_tuple,
                    Txid(invented_txid_data),
                    u32::try_from(event_index).expect("More than u32::MAX events in a block"),
                    index_block_hash,
                )
            })
            .collect();

        let operations = parsing_result.map_err(|e| BurnchainError::Bitcoin(e))?;

        Ok(RpcBlockData {
            operations,
            header: header.clone(),
        })
    }
}

impl BurnBlockIPC for RpcBlockData {
    type H = MockHeader;
    type B = RpcBlockData;

    fn height(&self) -> u64 {
        self.header.height
    }

    fn header(&self) -> Self::H {
        self.header.clone()
    }

    fn block(&self) -> Self::B {
        self.clone()
    }
}

impl BurnchainIndexer for RpcStacksIndexer {
    type P = RpcBlockParser;
    type B = RpcBlockData;
    type D = RpcBlockDownloader;

    fn connect(&mut self, _readwrite: bool) -> Result<(), BurnchainError> {
        // no-op
        Ok(())
    }

    fn get_channel(&self) -> Arc<dyn BurnchainChannel> {
        // no-op
        panic!("RPC indexer does not receive blocks through channel");
    }

    fn get_first_block_height(&self) -> u64 {
        self.first_block_height
    }

    fn get_first_block_header_hash(&self) -> Result<BurnchainHeaderHash, BurnchainError> {
        Ok(self.first_block_header_hash)
    }

    fn get_first_block_header_timestamp(&self) -> Result<u64, BurnchainError> {
        Ok(self.first_block_timestamp)
    }

    fn get_stacks_epochs(&self) -> Vec<StacksEpoch> {
        stacks::core::STACKS_EPOCHS_REGTEST.to_vec()
    }

    fn get_headers_path(&self) -> String {
        self.headers_path.clone()
    }

    fn get_highest_header_height(&self) -> Result<u64, BurnchainError> {
        match self.get_highest_header()? {
            Some(header) => Ok(header.height),
            None => Ok(0), // todo: I'm not sure if this is going to be cool with the burnchains module
        }
    }

    fn get_headers_height(&self) -> Result<u64, BurnchainError> {
        Ok(self.get_highest_header_height()? + 1)
    }

    fn find_chain_reorg(&mut self) -> Result<u64, BurnchainError> {
        for header in self.client.walk_headers(u64::MAX, None)? {
            let header_height = header.header.total_work.work;
            let my_header = self.get_header_at(header_height)?;
            // if we have a header at that height, check if its consistent with
            //  the header coming from the RPC client. If it isn't, keep iterating
            if let Some(my_header) = my_header {
                let header_index_hash = header.header.index_block_hash(&header.consensus_hash);
                if my_header.index_hash == header_index_hash {
                    // found a consistent point in the chain
                    return Ok(my_header.height);
                }
            }
        }

        // No common ancestor found!
        Err(BurnchainError::MissingHeaders)
    }

    fn sync_headers(
        &mut self,
        _start_height: u64,
        _end_height: Option<u64>,
    ) -> Result<u64, BurnchainError> {
        self.sync_all_headers()?;
        self.get_highest_header_height()
    }

    fn drop_headers(&mut self, new_height: u64) -> Result<(), BurnchainError> {
        // Noop. We never forget headers in this implementation.
        Ok(())
    }

    /// Get a range of block headers from db.
    /// If the range falls off the end of the db, then the returned array
    /// will be truncated to not include them.
    /// If the range does _not_ include `start_block`, then this method
    /// returns an empty array (even if there are headers in the range).
    fn read_headers(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> Result<Vec<MockHeader>, BurnchainError> {
        let query = "SELECT (height, index_hash, parent_index_hash, timestamp)
                     FROM headers WHERE height >= ? AND height < ? ORDER BY height";
        let headers: Vec<MockHeader> = query_rows(
            &self.db,
            query,
            params![u64_to_sql(start_block)?, u64_to_sql(end_block)?],
        )?;

        if let Some(first_header) = headers.get(0) {
            if first_header.height != start_block {
                return Ok(vec![]);
            }
        }

        Ok(headers)
    }

    fn downloader(&self) -> Self::D {
        todo!()
    }

    fn parser(&self) -> Self::P {
        todo!()
    }
}
