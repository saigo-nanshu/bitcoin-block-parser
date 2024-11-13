//! The [`BlockParser`] trait allows you to implement a custom parser or use one of the predefined ones.
//!
//! For example, imagine you want to sum the biggest tx using [`Transaction::total_size`] from
//! every block.  The simplest solution is to use the [`ParallelParser`] to loop over all the blocks.
//! ```no_run
//! use bitcoin_block_parser::*;
//! use std::convert::identity;
//!
//! let mut total: usize = 0;
//! for block in ParallelParser.parse_dir("/path/to/blocks", identity).unwrap() {
//!     let max = block.unwrap().txdata.into_iter().max_by_key(|tx| tx.total_size());
//!     total += max.unwrap().total_size();
//! }
//! println!("Total size: {}", total);
//! ```
//!
//! If you only want to run on a subset of blocks use [`HeaderParser`].  If you need to process
//! blocks in-order use [`InOrderParser`].
//! ```no_run
//! use bitcoin_block_parser::*;
//! use std::convert::identity;
//!
//! let headers = HeaderParser::parse("/path/to/blocks").unwrap();
//! // Skip the first 200,000 blocks
//! for block in InOrderParser.parse(&headers[200_000..], identity) {
//!   // Do whatever you need with the blocks in-order
//! }
//! ```
//!
//! Up until this point we have been using the `identity` function to return the [`Block`] directly.
//! However, your code will run much faster if you use a closure which maps the blocks to the
//! `total_size` in parallel.
//! ```no_run
//! use bitcoin_block_parser::*;
//!
//! let results = ParallelParser.parse_dir("/path/to/blocks", |block| {
//!     // Code in this closure runs in parallel
//!     let max = block.txdata.into_iter().max_by_key(|tx| tx.total_size());
//!     max.unwrap().total_size()
//! }).unwrap();
//!
//! let mut total: usize = results.into_iter().map(|size| size.unwrap()).sum();
//! println!("Total size: {}", total);
//! ```
//!
//! You can implement your own [`BlockParser`] which contains shared state using an [`Arc`].
//! Updating any locked stated should take place in [`BlockParser::batch`] to reduce the contention
//! on the lock.
//! ```no_run
//! use std::sync::*;
//! use bitcoin_block_parser::*;
//! use std::convert::identity;
//!
//! // Parser with shared state, must implement Clone for parallelism
//! #[derive(Clone, Default)]
//! struct SizeParser(Arc<Mutex<usize>>);
//!
//! // Custom implementation of a parser
//! impl BlockParser<usize> for SizeParser {
//!     // Runs in parallel on each block
//!     fn extract(&self, block: bitcoin::Block) -> Vec<usize> {
//!         let max = block.txdata.iter().max_by_key(|tx| tx.total_size());
//!         vec![max.unwrap().total_size()]
//!     }
//!
//!     // Runs on batches of items from the extract function
//!     fn batch(&self, items: Vec<usize>) -> Vec<usize> {
//!         // We should access our Mutex here to reduce contention on the lock
//!         let mut sum = self.0.lock().unwrap();
//!         for item in items {
//!             *sum += item;
//!         }
//!         vec![]
//!     }
//! }
//!
//! let parser = SizeParser::default();
//! for _ in parser.parse_dir("/path/to/blocks", identity).unwrap() {}
//! println!("Sum of txids: {:?}", parser.0);
//! ```

use crate::headers::ParsedHeader;
use crate::xor::XorReader;
use crate::HeaderParser;
use anyhow::Result;
use bitcoin::consensus::Decodable;
use bitcoin::{Block, Transaction};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::info;
use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use threadpool::ThreadPool;

/// Implement this trait to create a custom [`Block`] parser that returns type `B`.
pub trait BlockParser<B: Send + 'static>: Clone + Send + 'static {
    /// Extracts the data you need from the block.
    ///
    /// If you can keep `Vec<B>` small you will gain memory/speed performance.
    /// Always runs on blocks out-of-order using multiple threads so put compute-heavy code in here.
    fn extract(&self, block: Block) -> Vec<B>;

    /// Runs on batches of `B` to return the final results, blocks will be in-order if
    /// [`Options::order_output`] has been set.
    ///
    /// Implement batch if your algorithm depends on the order of the blocks or if you need to
    /// reduce lock contention when accessing shared state in `Arc<Mutex<_>>`.
    ///
    /// Use [`Options::batch_size`] if you need to tune the number of the `items`.
    fn batch(&self, items: Vec<B>) -> Vec<B> {
        items
    }

    /// The default [`Options`] that this parser will use.
    ///
    /// Implementing  [`BlockParser::options`] allows for tuning of the parameters of the algorithm.
    fn options() -> Options {
        Options::default()
    }

    /// Parse all the blocks located in the `blocks` directory
    ///
    /// Use the `map` closure to transform the final output.
    fn parse_dir<C: Send + 'static>(
        &self,
        blocks: &str,
        map: impl Fn(B) -> C + Clone + Send + 'static,
    ) -> Result<Receiver<Result<C>>> {
        let headers = HeaderParser::parse(blocks)?;
        Ok(self.parse(&headers, map))
    }

    /// Parse all the blocks represented by the headers.
    ///
    /// Use the `map` closure to transform the final output.
    fn parse<C: Send + 'static>(
        &self,
        headers: &[ParsedHeader],
        map: impl Fn(B) -> C + Clone + Send + 'static,
    ) -> Receiver<Result<C>> {
        self.parse_with_opts(headers, Self::options(), map)
    }

    /// Allows users to pass in custom [`Options`] in case they need to reduce memory usage or
    /// otherwise tune performance for their system.  Users should call [`BlockParser::options`]
    /// to get the default options associated with the parser first.
    fn parse_with_opts<C: Send + 'static>(
        &self,
        headers: &[ParsedHeader],
        opts: Options,
        map: impl Fn(B) -> C + Clone + Send + 'static,
    ) -> Receiver<Result<C>> {
        // Create the batches of headers
        let mut batched: Vec<Vec<ParsedHeader>> = vec![vec![]];
        for header in headers.iter().cloned() {
            let last = batched.last_mut().unwrap();
            last.push(header);
            if last.len() == opts.batch_size {
                batched.push(vec![]);
            }
        }

        // Run the extract function on multiple threads
        let start = Instant::now();
        let num_parsed = Arc::new(AtomicUsize::new(0));
        let (tx_b, rx_b) = bounded::<(usize, Result<Vec<B>>)>(opts.channel_buffer_size);
        let pool_extract = ThreadPool::new(opts.num_threads);
        for (index, headers) in batched.iter().cloned().enumerate() {
            let tx_b = tx_b.clone();
            let parser = self.clone();
            let num_parsed = num_parsed.clone();
            pool_extract.execute(move || {
                let mut batch_b: Vec<B> = vec![];
                for header in headers {
                    match parse_block(header) {
                        Err(e) => {
                            let _ = tx_b.send((index, Err(e)));
                        }
                        Ok(block) => batch_b.extend(parser.extract(block)),
                    }
                    increment_log(&num_parsed, start, opts.log_at);
                }
                let _ = tx_b.send((index, Ok(batch_b)));
            });
        }

        if opts.order_output {
            // Spawn a single thread to ensure the output is in order
            let (tx_c, rx_c) = bounded::<Result<C>>(opts.channel_buffer_size);
            let parser = self.clone();
            let map = map.clone();
            thread::spawn(move || {
                let mut current_index = 0;
                let mut unordered = FxHashMap::default();

                for (index, b) in rx_b {
                    unordered.insert(index, b);

                    while let Some(ordered) = unordered.remove(&current_index) {
                        current_index += 1;
                        parser.send_batch(&tx_c, ordered, map.clone());
                    }
                }
            });
            rx_c
        } else {
            // Spawn multiple threads in the case we don't care about the output order
            let pool_batch = ThreadPool::new(opts.num_threads);
            let (tx_c, rx_c) = bounded::<Result<C>>(opts.channel_buffer_size);
            for _ in 0..opts.num_threads {
                let tx_c = tx_c.clone();
                let rx_b = rx_b.clone();
                let parser = self.clone();
                let map = map.clone();
                pool_batch.execute(move || {
                    for (_, batch) in rx_b {
                        parser.send_batch(&tx_c, batch, map.clone());
                    }
                });
            }
            rx_c
        }
    }

    /// Helper function for sending batch results in a channel
    fn send_batch<C>(&self, tx_c: &Sender<Result<C>>, batch: Result<Vec<B>>, map: impl Fn(B) -> C) {
        let results = match batch.map(|b| self.batch(b)) {
            Ok(b) => b.into_iter().map(|b| Ok(map(b))).collect(),
            Err(e) => vec![Err(e)],
        };
        for result in results {
            let _ = tx_c.send(result);
        }
    }
}

/// Increments the number of blocks parsed, reporting the progress in a thread-safe manner
fn increment_log(num_parsed: &Arc<AtomicUsize>, start: Instant, log_at: usize) {
    let num = num_parsed.fetch_add(1, Ordering::Relaxed) + 1;

    if num % log_at == 0 {
        let elapsed = (Instant::now() - start).as_secs();
        let blocks = format!("{}K blocks parsed,", num / 1000);
        info!("{} {}m{}s elapsed", blocks, elapsed / 60, elapsed % 60);
    }
}

/// Parses a block from a `ParsedHeader` into a `bitcoin::Block`
fn parse_block(header: ParsedHeader) -> Result<Block> {
    let reader = XorReader::new(File::open(&header.path)?, header.xor_mask);
    let mut reader = BufReader::new(reader);
    reader.seek_relative(header.offset as i64)?;
    Ok(Block {
        header: header.inner,
        txdata: Vec::<Transaction>::consensus_decode_from_finite_reader(&mut reader)?,
    })
}

/// Parser that returns [`Block`] for users that don't implement a custom [`BlockParser`].
///
/// Runs in parallel over blocks making no guarantees about order.
#[derive(Clone, Debug)]
pub struct ParallelParser;
impl BlockParser<Block> for ParallelParser {
    fn extract(&self, block: Block) -> Vec<Block> {
        vec![block]
    }

    fn options() -> Options {
        // since we do no batch processing, set batch_size to 1 to reduce memory usage
        Options::default().batch_size(1)
    }
}

/// Parse all the blocks represented by the headers, ensuring the blocks are returned
/// in the same order the [`ParsedHeader`] were passed in.
///
/// Note that by ordering the results [`BlockParser::batch`] will run on a single thread instead
/// of multiple which could affect performance.
#[derive(Clone, Debug)]
pub struct InOrderParser;
impl BlockParser<Block> for InOrderParser {
    fn extract(&self, block: Block) -> Vec<Block> {
        vec![block]
    }

    fn options() -> Options {
        // since we do no batch processing, set batch_size to 1 to reduce memory usage
        Options::default().batch_size(1).order_output()
    }
}

/// Options to tune the performance of the parser, generally you can stick to the defaults unless
/// you run into memory issues.
pub struct Options {
    order_output: bool,
    num_threads: usize,
    batch_size: usize,
    channel_buffer_size: usize,
    log_at: usize,
}
/// Defaults that should be close to optimal for most parsers
///
/// `order_output` determines whether the results will be returned in-order
/// `num_threads: 128` should be enough for most systems regardless of disk speed
/// `batch_size: 10` improves batch performance without using too much memory
/// `channel_buffer_size: 100` increasing beyond this usually just increases memory usage
/// `log_at: 10_000` will produce logs every few seconds without spamming output
impl Default for Options {
    fn default() -> Self {
        Self {
            order_output: false,
            num_threads: 128,
            batch_size: 10,
            channel_buffer_size: 100,
            log_at: 10_000,
        }
    }
}
impl Options {
    /// Ensures that the output of the [`BlockParser::parse`] function will be in same order as the
    /// [`BlockHeader`] passed in.
    ///
    /// [`BlockParser::batch`] will receive blocks in-order, however this requires running that
    /// function on a single thread, rather than multiple threads.
    pub fn order_output(mut self) -> Self {
        self.order_output = true;
        self
    }

    /// Set the number of threads to handle the processing steps.
    ///
    /// Typically limited by disk I/O and the number of threads your system can handle,
    /// increasing it generally improves speed at the cost of memory usage.
    pub fn num_threads(mut self, n: usize) -> Self {
        assert!(n > 0);
        self.num_threads = n;
        self
    }

    /// Number of items passed into [`BlockParser::batch`].
    ///
    /// If you need to access shared state through an `Arc<Mutex<_>>` a bigger batch size can
    /// improve performance, at the cost of more memory depending on the size of [`BlockParser::extract`]
    pub fn batch_size(mut self, n: usize) -> Self {
        assert!(n > 0);
        self.batch_size = n;
        self
    }

    /// Set the number of size of the buffers used between channels.
    ///
    /// Doesn't have a significant impact on speed/memory so long as it's set high enough.
    pub fn channel_buffer_size(mut self, n: usize) -> Self {
        assert!(n > 0);
        self.channel_buffer_size = n;
        self
    }

    /// Set how many blocks to parse before printing a log message out.
    ///
    /// To disable logging, simply set `log_at` to `usize::MAX`, required to be at least 1K
    pub fn log_at(mut self, n: usize) -> Self {
        assert!(n >= 1000);
        self.log_at = n;
        self
    }
}
