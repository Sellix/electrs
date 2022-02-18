use anyhow::{Context, Result};

use bitcoin::{
    consensus::{serialize, Decodable},
    hashes::hex::ToHex,
    Amount, Block, BlockHash, Transaction, Txid,
};
use bitcoincore_rpc::{json, jsonrpc, Auth, Client, RpcApi};
use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use serde_json::{json, value::RawValue, Value};

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::{
    chain::Chain,
    config::Config,
    metrics::Metrics,
    p2p::Connection,
    signals::ExitFlag,
    types::{FilePosition, HeaderRow},
};

enum PollResult {
    Done(Result<()>),
    Retry,
}

fn rpc_poll(client: &mut Client) -> PollResult {
    match client.get_blockchain_info() {
        Ok(info) => {
            let left_blocks = info.headers - info.blocks;
            if info.initial_block_download || left_blocks > 0 {
                info!(
                    "waiting for {} blocks to download{}",
                    left_blocks,
                    if info.initial_block_download {
                        " (IBD)"
                    } else {
                        ""
                    }
                );
                return PollResult::Retry;
            }
            PollResult::Done(Ok(()))
        }
        Err(err) => {
            if let Some(e) = extract_bitcoind_error(&err) {
                if e.code == -28 {
                    info!("waiting for RPC warmup: {}", e.message);
                    return PollResult::Retry;
                }
            }
            PollResult::Done(Err(err).context("daemon not available"))
        }
    }
}

fn read_cookie(path: &Path) -> Result<(String, String)> {
    // Load username and password from bitcoind cookie file:
    // * https://github.com/bitcoin/bitcoin/pull/6388/commits/71cbeaad9a929ba6a7b62d9b37a09b214ae00c1a
    // * https://bitcoin.stackexchange.com/questions/46782/rpc-cookie-authentication
    let mut file = File::open(path)
        .with_context(|| format!("failed to open bitcoind cookie file: {}", path.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read bitcoind cookie from {}", path.display()))?;

    let parts: Vec<&str> = contents.splitn(2, ':').collect();
    ensure!(
        parts.len() == 2,
        "failed to parse bitcoind cookie - missing ':' separator"
    );
    Ok((parts[0].to_owned(), parts[1].to_owned()))
}

fn rpc_connect(config: &Config) -> Result<Client> {
    let rpc_url = format!("http://{}", config.daemon_rpc_addr);
    // Allow `wait_for_new_block` to take a bit longer before timing out.
    // See https://github.com/romanz/electrs/issues/495 for more details.
    let builder = jsonrpc::simple_http::SimpleHttpTransport::builder()
        .url(&rpc_url)?
        .timeout(config.jsonrpc_timeout);
    let builder = match config.daemon_auth.get_auth() {
        Auth::None => builder,
        Auth::UserPass(user, pass) => builder.auth(user, Some(pass)),
        Auth::CookieFile(path) => {
            let (user, pass) = read_cookie(&path)?;
            builder.auth(user, Some(pass))
        }
    };
    Ok(Client::from_jsonrpc(jsonrpc::Client::with_transport(
        builder.build(),
    )))
}

pub(crate) struct FileReader {
    blocks_dir: PathBuf,
}

impl FileReader {
    pub(crate) fn open(&self, pos: FilePosition) -> Result<File> {
        let name = format!("blk{:05}.dat", pos.file_id);
        let path = self.blocks_dir.join(name);
        let mut file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        file.seek(SeekFrom::Start(u64::from(pos.offset)))?;
        Ok(file)
    }
}

pub(crate) struct BlockHashPosition {
    pub(crate) hash: BlockHash,
    pub(crate) pos: FilePosition,
}

impl BlockHashPosition {
    fn new(hash: BlockHash, pos: FilePosition) -> Self {
        Self { hash, pos }
    }
}

pub struct Daemon {
    p2p: Mutex<Connection>,
    rpc: Client,
    reader: FileReader,
}

impl Daemon {
    pub(crate) fn connect(
        config: &Config,
        exit_flag: &ExitFlag,
        metrics: &Metrics,
    ) -> Result<Self> {
        let mut rpc = rpc_connect(config)?;

        loop {
            exit_flag
                .poll()
                .context("bitcoin RPC polling interrupted")?;
            match rpc_poll(&mut rpc) {
                PollResult::Done(result) => {
                    result.context("bitcoind RPC polling failed")?;
                    break; // on success, finish polling
                }
                PollResult::Retry => {
                    std::thread::sleep(std::time::Duration::from_secs(1)); // wait a bit before polling
                }
            }
        }

        let network_info = rpc.get_network_info()?;
        if network_info.version < 21_00_00 {
            bail!("electrs requires bitcoind 0.21+");
        }
        if !network_info.network_active {
            bail!("electrs requires active bitcoind p2p network");
        }
        let info = rpc.get_blockchain_info()?;
        if info.pruned {
            bail!("electrs requires non-pruned bitcoind node");
        }

        let p2p = Mutex::new(Connection::connect(
            config.network,
            config.daemon_p2p_addr,
            metrics,
        )?);
        let reader = FileReader {
            blocks_dir: config.blocks_dir.clone(),
        };
        let daemon = Self { p2p, rpc, reader };
        // Make sure `getblocklocations` RPC is available (and test it with the latest block)
        daemon.verify_blocks(&[info.best_block_hash])?;
        Ok(daemon)
    }

    pub(crate) fn estimate_fee(&self, nblocks: u16) -> Result<Option<Amount>> {
        Ok(self
            .rpc
            .estimate_smart_fee(nblocks, None)
            .context("failed to estimate fee")?
            .fee_rate)
    }

    pub(crate) fn get_relay_fee(&self) -> Result<Amount> {
        Ok(self
            .rpc
            .get_network_info()
            .context("failed to get relay fee")?
            .relay_fee)
    }

    pub(crate) fn broadcast(&self, tx: &Transaction) -> Result<Txid> {
        self.rpc
            .send_raw_transaction(tx)
            .context("failed to broadcast transaction")
    }

    pub(crate) fn get_transaction_info(
        &self,
        txid: &Txid,
        blockhash: Option<BlockHash>,
    ) -> Result<Value> {
        // No need to parse the resulting JSON, just return it as-is to the client.
        self.rpc
            .call(
                "getrawtransaction",
                &[json!(txid), json!(true), json!(blockhash)],
            )
            .context("failed to get transaction info")
    }

    pub(crate) fn get_transaction_hex(
        &self,
        txid: &Txid,
        blockhash: Option<BlockHash>,
    ) -> Result<Value> {
        let tx = self.get_transaction(txid, blockhash)?;
        Ok(json!(serialize(&tx).to_hex()))
    }

    pub(crate) fn get_transaction(
        &self,
        txid: &Txid,
        blockhash: Option<BlockHash>,
    ) -> Result<Transaction> {
        self.rpc
            .get_raw_transaction(txid, blockhash.as_ref())
            .context("failed to get transaction")
    }

    pub(crate) fn get_block_txids(&self, blockhash: BlockHash) -> Result<Vec<Txid>> {
        Ok(self
            .rpc
            .get_block_info(&blockhash)
            .context("failed to get block txids")?
            .tx)
    }

    pub(crate) fn get_mempool_txids(&self) -> Result<Vec<Txid>> {
        self.rpc
            .get_raw_mempool()
            .context("failed to get mempool txids")
    }

    fn get_existing<T: for<'a> serde::de::Deserialize<'a>, U>(
        &self,
        command: &str,
        txids: impl Iterator<Item = Txid>,
        map_fn: impl Fn(T) -> Result<U>,
    ) -> Result<HashMap<Txid, U>> {
        let client = self.rpc.get_jsonrpc_client();
        let txids: Vec<Txid> = txids.collect();
        if txids.is_empty() {
            return Ok(Default::default());
        }
        let args_vec: Vec<Vec<Box<RawValue>>> =
            txids.iter().map(|txid| vec![jsonrpc::arg(txid)]).collect();
        let requests: Vec<jsonrpc::Request> = args_vec
            .iter()
            .map(|args| client.build_request(command, args))
            .collect();
        let responses = client
            .send_batch(&requests)
            .with_context(|| format!("{} failed", command))?;
        Ok(responses
            .into_iter()
            .zip(txids.into_iter())
            .filter_map(|(response, txid)| response.map(|r| (r, txid))) // drop missing entries
            .filter_map(|(response, txid)| match response.result::<T>() {
                Ok(response) => match map_fn(response) {
                    Ok(r) => Some((txid, r)),
                    Err(err) => {
                        warn!("{} {} failed to convert response: {:?}", command, txid, err); // drop failed responses
                        None
                    }
                },
                Err(err) => {
                    warn!("{} {} failed: {:?}", command, txid, err); // drop failed responses
                    None
                }
            })
            .collect())
    }

    fn tx_from_hex(hex: String) -> Result<Transaction> {
        let bytes: Vec<u8> = bitcoin::hashes::hex::FromHex::from_hex(&hex)?;
        Ok(bitcoin::consensus::encode::deserialize(&bytes)?)
    }

    pub(crate) fn get_existing_mempool_entries(
        &self,
        txids: impl Iterator<Item = Txid>,
    ) -> Result<HashMap<Txid, json::GetMempoolEntryResult>> {
        self.get_existing("getmempoolentry", txids, |entry| Ok(entry))
    }

    pub(crate) fn get_existing_transactions(
        &self,
        txids: impl Iterator<Item = Txid>,
    ) -> Result<HashMap<Txid, Transaction>> {
        self.get_existing("getrawtransaction", txids, Daemon::tx_from_hex)
    }

    fn get_block_locations(&self, blockhashes: &[BlockHash]) -> Result<Vec<FilePosition>> {
        self.rpc
            .call("getblocklocations", &[json!(blockhashes)])
            .context("failed to get block locations")
    }

    fn read_block(&self, blockhash: BlockHash) -> Result<(Block, FilePosition)> {
        let locations = self.get_block_locations(&[blockhash])?;
        assert_eq!(locations.len(), 1);
        let pos = locations[0];
        let block = Block::consensus_decode(&mut self.open_file(pos)?)?;
        Ok((block, pos))
    }

    pub(crate) fn verify_blocks(&self, blockhashes: &[BlockHash]) -> Result<()> {
        for blockhash in blockhashes {
            let (block, pos) = self.read_block(*blockhash)?;
            ensure!(block.block_hash() == *blockhash, "incorrect block loaded");
            debug!("verified block {} at {:?}", blockhash, pos);
        }
        Ok(())
    }

    pub(crate) fn get_genesis(&self) -> Result<HeaderRow> {
        let hash = self.rpc.get_block_hash(0)?;
        let (block, pos) = self.read_block(hash)?;
        let size = u32::try_from(serialize(&block).len())?;
        Ok(HeaderRow {
            header: block.header,
            hash,
            pos,
            size,
        })
    }

    pub(crate) fn get_new_headers(&self, chain: &Chain) -> Result<Vec<BlockHashPosition>> {
        let blockhashes = self.p2p.lock().get_new_headers(chain)?;
        let positions = self.get_block_locations(&blockhashes)?;
        assert_eq!(blockhashes.len(), positions.len());
        Ok(blockhashes
            .into_iter()
            .zip(positions.into_iter())
            .map(|(blockhash, position)| BlockHashPosition::new(blockhash, position))
            .collect())
    }

    pub(crate) fn open_file(&self, pos: FilePosition) -> Result<File> {
        self.reader.open(pos)
    }

    pub(crate) fn new_block_notification(&self) -> Receiver<()> {
        self.p2p.lock().new_block_notification()
    }
}

pub(crate) type RpcError = bitcoincore_rpc::jsonrpc::error::RpcError;

pub(crate) fn extract_bitcoind_error(err: &bitcoincore_rpc::Error) -> Option<&RpcError> {
    use bitcoincore_rpc::{
        jsonrpc::error::Error::Rpc as ServerError, Error::JsonRpc as JsonRpcError,
    };
    match err {
        JsonRpcError(ServerError(e)) => Some(e),
        _ => None,
    }
}
