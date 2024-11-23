//! Contains [`UtxoParser`] for tracking input amounts and output statuses in [`UtxoBlock`].

use crate::blocks::{BlockParser, ParserIterator, ParserOptions, Pipeline};
use anyhow::{bail, Result};
use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::{Amount, Block, OutPoint, Transaction, TxIn, TxOut, Txid};
use dashmap::DashMap;
use log::info;
use rand::prelude::SmallRng;
use rand::{Error, RngCore, SeedableRng};
use scalable_cuckoo_filter::{DefaultHasher, ScalableCuckooFilter, ScalableCuckooFilterBuilder};
use std::fs;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::iter::Zip;
use std::slice::Iter;
use std::sync::{Arc, Mutex};

/// A block that has been parsed tracking input amounts and output status
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct UtxoBlock {
    /// The block header
    pub header: Header,
    /// List of transactions contained in the block
    pub txdata: Vec<UtxoTransaction>,
}

impl UtxoBlock {
    /// Construct from a bitcoin [`Block`].
    fn new(block: Block) -> Self {
        Self {
            header: block.header,
            txdata: block.txdata.into_iter().map(UtxoTransaction::new).collect(),
        }
    }

    /// Convert back into a [`bitcoin::Block`].
    pub fn to_block(self) -> Block {
        Block {
            header: self.header,
            txdata: self.txdata.into_iter().map(|tx| tx.transaction).collect(),
        }
    }
}

/// A transaction that has been parsed tracking input amounts and output status
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct UtxoTransaction {
    /// Underlying bitcoin transaction [`Transaction`]
    pub transaction: Transaction,
    /// Precomputed [`Txid`]
    pub txid: Txid,
    /// Tracks the input amounts in-order of inputs
    inputs: Vec<Amount>,
    /// Tracks the output statuses in-order of outputs
    outputs: Vec<OutputStatus>,
}

impl UtxoTransaction {
    /// Construct from a bitcoin [`Transaction`].
    fn new(transaction: Transaction) -> UtxoTransaction {
        Self {
            txid: transaction.compute_txid(),
            transaction,
            inputs: vec![],
            outputs: vec![],
        }
    }

    /// Returns the [`TxIn`] of the transaction zipped with the input amounts.
    pub fn input(&self) -> Zip<Iter<'_, TxIn>, Iter<'_, Amount>> {
        self.transaction.input.iter().zip(self.inputs.iter())
    }

    /// Returns the [`TxOut`] of the transaction zipped with the output [`OutputStatus`].
    pub fn output(&self) -> Zip<Iter<'_, TxOut>, Iter<'_, OutputStatus>> {
        self.transaction.output.iter().zip(self.outputs.iter())
    }
}

/// Status of the [`TxOut`] within the transaction graph.
#[derive(Clone, Debug, Eq, PartialEq, Copy)]
pub enum OutputStatus {
    /// The output was spent in a later block.
    Spent,
    /// The output was never spent in any later block (it is a UTXO).
    Unspent,
    /// The status of the output is unknown (only if [`UtxoParser::load_filter`] was not called).
    Unknown,
}

type ShortOutPoints = (Vec<ShortOutPoint>, Vec<ShortOutPoint>);
type ShortOutPointFilter = ScalableCuckooFilter<ShortOutPoint, DefaultHasher, FastRng>;

/// Multithreaded parser that returns a [`ParserIterator`] of [`UtxoBlock`].
/// * Tracks the [`Amount`] for every [`TxIn`].
/// * Tracks the [`OutputStatus`] for every [`TxOut`] if [`UtxoParser::load_filter`] is called.
///
/// # Examples
/// Computing the largest mining fee requires knowing the input amounts of every transaction.
/// Call [`UtxoParser::parse`] to get a [`UtxoBlock`] that tracks input amounts.
/// ```no_run
/// use std::cmp::max;
/// use bitcoin::Amount;
/// use bitcoin_block_parser::utxos::*;
///
/// let parser = UtxoParser::new("/home/user/.bitcoin/blocks/").unwrap();
/// let fees = parser.parse().map_parallel(|block| {
///     let mut max_mining_fee = Amount::ZERO;
///     for tx in block.txdata.into_iter() {
///         // For every transaction sum up the input and output amounts
///         let inputs: Amount = tx.input().map(|(_, amount)| *amount).sum();
///         let outputs: Amount = tx.output().map(|(out, _)| out.value).sum();
///         if !tx.transaction.is_coinbase() {
///             // Subtract outputs amount from inputs amount to get the fee
///             max_mining_fee = max(inputs - outputs, max_mining_fee);
///         }
///     }
///     max_mining_fee
/// });
/// println!("Maximum mining fee: {}", fees.max().unwrap());
/// ```
///
/// Computing the largest UTXO requires knowing the [`OutputStatus`] to determine whether a
/// [`TxOut`] was spent or unspent.  Call [`UtxoParser::load_or_create_filter`] to track the output
/// status.
///
/// Although this takes longer to run the first time it also lowers the memory usage.
/// ```no_run
/// use std::cmp::max;
/// use bitcoin::Amount;
/// use bitcoin_block_parser::utxos::*;
///
/// let parser = UtxoParser::new("/home/user/.bitcoin/blocks/").unwrap();
/// let blocks = parser.load_or_create_filter("filter.bin").unwrap().parse();
/// let amounts = blocks.map_parallel(|block| {
///     let mut max_unspent_tx = Amount::ZERO;
///     for tx in block.txdata.into_iter() {
///         for (output, status) in tx.output() {
///             if status == &OutputStatus::Unspent {
///                 max_unspent_tx = max(output.value, max_unspent_tx);
///             }
///         }
///     }
///     max_unspent_tx
/// });
/// println!("Maximum unspent output: {}", amounts.max().unwrap());
/// ```
pub struct UtxoParser {
    /// Filter that contains all unspent transaction outpoints.
    filter: Option<ShortOutPointFilter>,
    /// Underlying parser for parsing the blocks.
    parser: BlockParser,
    /// Used to allocate the initial capacity of shared state.
    estimated_utxos: usize,
}

impl UtxoParser {
    /// Creates a new parser given the `blocks` directory where the `*.blk` files are located.
    ///
    /// - Returns an `Err` if unable to parse the `blk` files.
    /// - You can [specify the blocks directory](https://en.bitcoin.it/wiki/Data_directory) when
    ///   running `bitcoind`.
    pub fn new(blocks_dir: &str) -> Result<Self> {
        Self::new_with_opts(blocks_dir, ParserOptions::default())
    }

    /// Creates a parser with custom [`ParserOptions`].
    pub fn new_with_opts(blocks_dir: &str, options: ParserOptions) -> Result<Self> {
        Ok(Self {
            filter: None,
            parser: BlockParser::new_with_opts(blocks_dir, options)?,
            estimated_utxos: 300_000_000,
        })
    }

    /// Set the estimated amount of UTXOs in the range of blocks you are parsing.
    ///
    /// Used to lower the memory usage of shared state objects.
    pub fn estimated_utxos(mut self, estimated_utxos: usize) -> Self {
        self.estimated_utxos = estimated_utxos;
        self
    }

    /// Parse the blocks into an iterator of [`UtxoBlock`].
    pub fn parse(self) -> ParserIterator<UtxoBlock> {
        // if using a filter we can save memory by reducing the initial hashmap capacity
        let hashmap_capacity = if self.filter.is_some() {
            self.estimated_utxos / 10
        } else {
            self.estimated_utxos
        };
        let pipeline = UtxoPipeline::new(self.filter, hashmap_capacity);
        self.parser
            .parse(UtxoBlock::new)
            .ordered()
            .pipeline(&pipeline)
    }

    /// Set the height of the last block to parse.
    ///
    /// Parsing always starts at the genesis block in order to track the transaction graph properly.
    pub fn block_range_end(mut self, end: usize) -> Self {
        self.parser = self.parser.block_range(0, end);
        self
    }

    /// Loads a `filter_file` or creates a new one if it doesn't exist.
    pub fn load_or_create_filter(self, filter_file: &str) -> Result<Self> {
        if !fs::exists(filter_file)? {
            self.create_filter(filter_file)?.load_filter(filter_file)
        } else {
            self.load_filter(filter_file)
        }
    }

    /// Loads a `filter_file` or returns `Err` if it doesn't exist.
    pub fn load_filter(mut self, filter_file: &str) -> Result<Self> {
        if !fs::exists(filter_file)? {
            bail!("Filter file '{}' doesn't exist", filter_file);
        }
        let reader = BufReader::new(File::open(filter_file)?);
        let filter = bincode::deserialize_from(reader)?;

        self.filter = Some(filter);
        Ok(self)
    }

    /// Creates a new `filter_file`.
    pub fn create_filter(self, filter_file: &str) -> Result<Self> {
        info!("Creating '{}'", filter_file);
        let filter = UtxoFilter::new(self.estimated_utxos);
        self.parser
            .parse(UtxoFilter::outpoints)
            .ordered()
            .map(&|outpoints| filter.update(outpoints))
            .for_each(|_| {});

        let filter = Arc::try_unwrap(filter.filter).expect("Arc still referenced");
        let mut filter = Mutex::into_inner(filter)?;
        filter.shrink_to_fit();
        let writer = BufWriter::new(File::create(filter_file)?);
        bincode::serialize_into(writer, &filter)?;
        Ok(self)
    }
}

/// Contains the filter data that tracks all unspent outputs in a memory-efficient manner.
#[derive(Clone)]
struct UtxoFilter {
    filter: Arc<Mutex<ShortOutPointFilter>>,
}

impl UtxoFilter {
    /// Construct with an initial `filter_capacity`.
    fn new(filter_capacity: usize) -> UtxoFilter {
        Self {
            filter: Arc::new(Mutex::new(
                ScalableCuckooFilterBuilder::default()
                    .initial_capacity(filter_capacity)
                    .false_positive_probability(0.000_000_000_001)
                    .rng(FastRng::default())
                    .finish(),
            )),
        }
    }

    /// Returns [`ShortOutPoint`] for all inputs and outputs.
    fn outpoints(block: Block) -> ShortOutPoints {
        let mut inputs = vec![];
        let mut outputs = vec![];
        for tx in block.txdata.iter() {
            let txid = tx.compute_txid();
            for input in &tx.input {
                inputs.push(ShortOutPoint::from_outpoint(&input.previous_output));
            }

            for (index, _) in tx.output.iter().enumerate() {
                outputs.push(ShortOutPoint::new(index, &txid));
            }
        }
        (inputs, outputs)
    }

    /// Given the results of `outpoints()` update the filter.
    pub fn update(&self, outpoints: ShortOutPoints) {
        let mut filter = self.filter.lock().expect("Lock poisoned");
        let (inputs, outputs) = outpoints;
        for outpoint in outputs {
            // insert outpoints for every output
            filter.insert(&outpoint);
        }
        for input in inputs {
            // remove outpoints that are spent in a subsequent transaction
            filter.remove(&input);
        }
    }
}

/// Pipeline for multithreaded tracking of the input amounts and output statuses.
#[derive(Clone, Default)]
struct UtxoPipeline {
    /// Optional filter containing all unspent outpoints.
    filter: Option<Arc<ShortOutPointFilter>>,
    /// Tracks the amounts for every input.
    amounts: Arc<DashMap<ShortOutPoint, Amount>>,
}

impl UtxoPipeline {
    /// Construct a new pipeline with an optional `filter` and initial `hashmap_capacity`.
    fn new(filter: Option<ShortOutPointFilter>, hashmap_capacity: usize) -> Self {
        Self {
            filter: filter.map(Arc::new),
            amounts: Arc::new(DashMap::with_capacity(hashmap_capacity)),
        }
    }

    /// Returns the [`OutputStatus`] of an outpoint, returning [`OutputStatus::Unknown`] if running
    /// without a filter.
    fn status(&self, outpoint: &ShortOutPoint) -> OutputStatus {
        match &self.filter {
            None => OutputStatus::Unknown,
            Some(filter) if filter.contains(outpoint) => OutputStatus::Unspent,
            _ => OutputStatus::Spent,
        }
    }
}

impl Pipeline<UtxoBlock, UtxoBlock, UtxoBlock> for UtxoPipeline {
    fn first(&self, mut block: UtxoBlock) -> UtxoBlock {
        for tx in &mut block.txdata {
            for (index, output) in tx.transaction.output.iter().enumerate() {
                let outpoint = ShortOutPoint::new(index, &tx.txid);
                let status = self.status(&outpoint);
                // if an outpoint is unspent we don't need to track it (saving memory)
                if status != OutputStatus::Unspent {
                    self.amounts.insert(outpoint, output.value);
                }
                tx.outputs.push(status);
            }
        }
        block
    }

    fn second(&self, mut block: UtxoBlock) -> UtxoBlock {
        for tx in &mut block.txdata {
            for input in tx.transaction.input.iter() {
                if tx.transaction.is_coinbase() {
                    // coinbase transactions will not have a previous input
                    tx.inputs.push(Amount::ZERO);
                } else {
                    let outpoint = ShortOutPoint::from_outpoint(&input.previous_output);
                    let (_, value) = self.amounts.remove(&outpoint).expect("Missing outpoint");
                    tx.inputs.push(value);
                }
            }
        }
        block
    }
}

/// Shortened [`OutPoint`] to save memory (14 bytes instead of 36 bytes)
///
/// - 2 bytes represent far more than the maximum tx outputs (2^16)
/// - 12 byte subset of the txid is unlikely to generate collisions even with 1 billion txs (~6.3e-12)
#[derive(Eq, PartialEq, Hash, Debug, Clone)]
struct ShortOutPoint(pub Vec<u8>);
impl ShortOutPoint {
    /// Shorten an existing [`OutPoint`].
    fn from_outpoint(outpoint: &OutPoint) -> ShortOutPoint {
        Self::new(outpoint.vout as usize, &outpoint.txid)
    }

    /// Create a new [`ShortOutPoint`] given its transaction id and index.
    fn new(vout: usize, txid: &Txid) -> ShortOutPoint {
        let mut bytes = vec![];
        bytes.extend_from_slice(&vout.to_le_bytes()[0..2]);
        bytes.extend_from_slice(&txid.as_byte_array()[0..12]);
        ShortOutPoint(bytes)
    }
}

/// Wrapper for [`SmallRng`] since it doesn't implement [`Default`] required to deserialize.
#[derive(Debug)]
struct FastRng(SmallRng);
impl Default for FastRng {
    fn default() -> Self {
        Self(SmallRng::seed_from_u64(0x2c76c58e13b3a812))
    }
}
impl RngCore for FastRng {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), Error> {
        self.0.try_fill_bytes(dest)
    }
}
