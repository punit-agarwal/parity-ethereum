// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

///
/// Blockchain downloader
///

use std::collections::{HashSet, VecDeque};
use std::cmp;
use heapsize::HeapSizeOf;
use ethereum_types::H256;
use rlp::{self, Rlp};
use ethcore::header::BlockNumber;
use ethcore::client::{BlockStatus, BlockId};
use ethcore::error::{ImportErrorKind, QueueErrorKind, BlockError, Error as EthcoreError, ErrorKind as EthcoreErrorKind};
use sync_io::SyncIo;
use blocks::{BlockCollection, SyncBody, SyncHeader};
use chain::BlockSet;

const MAX_HEADERS_TO_REQUEST: usize = 128;
const MAX_BODIES_TO_REQUEST: usize = 32;
const MAX_RECEPITS_TO_REQUEST: usize = 128;
const SUBCHAIN_SIZE: u64 = 256;
const MAX_ROUND_PARENTS: usize = 16;
const MAX_PARALLEL_SUBCHAIN_DOWNLOAD: usize = 5;
const MAX_USELESS_HEADERS_PER_ROUND: usize = 3;

// logging macros prepend BlockSet context for log filtering
macro_rules! trace_sync {
	($self:ident, $fmt:expr, $($arg:tt)+) => {
		trace!(target: "sync", concat!("{:?}: ", $fmt), $self.block_set, $($arg)+);
	};
	($self:ident, $fmt:expr) => {
		trace!(target: "sync", concat!("{:?}: ", $fmt), $self.block_set);
	};
}

macro_rules! debug_sync {
	($self:ident, $fmt:expr, $($arg:tt)+) => {
		debug!(target: "sync", concat!("{:?}: ", $fmt), $self.block_set, $($arg)+);
	};
	($self:ident, $fmt:expr) => {
		debug!(target: "sync", concat!("{:?}: ", $fmt), $self.block_set);
	};
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
/// Downloader state
pub enum State {
	/// No active downloads.
	Idle,
	/// Downloading subchain heads
	ChainHead,
	/// Downloading blocks
	Blocks,
	/// Download is complete
	Complete,
}

/// Data that needs to be requested from a peer.
pub enum BlockRequest {
	Headers {
		start: H256,
		count: u64,
		skip: u64,
	},
	Bodies {
		hashes: Vec<H256>,
	},
	Receipts {
		hashes: Vec<H256>,
	},
}

/// Indicates sync action
#[derive(Eq, PartialEq, Debug)]
pub enum DownloadAction {
	/// Do nothing
	None,
	/// Reset downloads for all peers
	Reset
}

#[derive(Eq, PartialEq, Debug)]
pub enum BlockDownloaderImportError {
	/// Imported data is rejected as invalid. Peer should be dropped.
	Invalid,
	/// Imported data is valid but rejected cause the downloader does not need it.
	Useless,
}

impl From<rlp::DecoderError> for BlockDownloaderImportError {
	fn from(_: rlp::DecoderError) -> BlockDownloaderImportError {
		BlockDownloaderImportError::Invalid
	}
}

/// Block downloader strategy.
/// Manages state and block data for a block download process.
pub struct BlockDownloader {
	/// Which set of blocks to download
	block_set: BlockSet,
	/// Downloader state
	state: State,
	/// Highest block number seen
	highest_block: Option<BlockNumber>,
	/// Downloaded blocks, holds `H`, `B` and `S`
	blocks: BlockCollection,
	/// Last imported block number
	last_imported_block: BlockNumber,
	/// Last imported block hash
	last_imported_hash: H256,
	/// Number of blocks imported this round
	imported_this_round: Option<usize>,
	/// Block number the last round started with.
	last_round_start: BlockNumber,
	last_round_start_hash: H256,
	/// Block parents imported this round (hash, parent)
	round_parents: VecDeque<(H256, H256)>,
	/// Do we need to download block recetips.
	download_receipts: bool,
	/// Sync up to the block with this hash.
	target_hash: Option<H256>,
	/// Probing range for seeking common best block.
	retract_step: u64,
	/// Whether reorg should be limited.
	limit_reorg: bool,
	/// consecutive useless headers this round
	useless_headers_count: usize,
}

impl BlockDownloader {
	/// Create a new instance of syncing strategy.
	/// For BlockSet::NewBlocks this won't reorganize to before the last kept state.
	pub fn new(block_set: BlockSet, start_hash: &H256, start_number: BlockNumber) -> Self {
		let (limit_reorg, sync_receipts) = match block_set {
			BlockSet::NewBlocks => (true, false),
			BlockSet::OldBlocks => (false, true)
		};
		BlockDownloader {
			block_set: block_set,
			state: State::Idle,
			highest_block: None,
			last_imported_block: start_number,
			last_imported_hash: start_hash.clone(),
			last_round_start: start_number,
			last_round_start_hash: start_hash.clone(),
			blocks: BlockCollection::new(sync_receipts),
			imported_this_round: None,
			round_parents: VecDeque::new(),
			download_receipts: sync_receipts,
			target_hash: None,
			retract_step: 1,
			limit_reorg: limit_reorg,
			useless_headers_count: 0,
		}
	}

	/// Reset sync. Clear all local downloaded data.
	pub fn reset(&mut self) {
		self.blocks.clear();
		self.useless_headers_count = 0;
		self.state = State::Idle;
	}

	/// Mark a block as known in the chain
	pub fn mark_as_known(&mut self, hash: &H256, number: BlockNumber) {
		if number >= self.last_imported_block + 1 {
			self.last_imported_block = number;
			self.last_imported_hash = hash.clone();
			self.imported_this_round = Some(self.imported_this_round.unwrap_or(0) + 1);
			self.last_round_start = number;
			self.last_round_start_hash = hash.clone();
		}
	}

	/// Check if download is complete
	pub fn is_complete(&self) -> bool {
		self.state == State::Complete
	}

	/// Check if particular block hash is being downloaded
	pub fn is_downloading(&self, hash: &H256) -> bool {
		self.blocks.is_downloading(hash)
	}

	/// Set starting sync block
	pub fn set_target(&mut self, hash: &H256) {
		self.target_hash = Some(hash.clone());
	}

	/// Unmark header as being downloaded.
	pub fn clear_header_download(&mut self, hash: &H256) {
		self.blocks.clear_header_download(hash)
	}

	/// Unmark block body as being downloaded.
	pub fn clear_body_download(&mut self, hashes: &[H256]) {
		self.blocks.clear_body_download(hashes)
	}

	/// Unmark block receipt as being downloaded.
	pub fn clear_receipt_download(&mut self, hashes: &[H256]) {
		self.blocks.clear_receipt_download(hashes)
	}
	/// Reset collection for a new sync round with given subchain block hashes.
	pub fn reset_to(&mut self, hashes: Vec<H256>) {
		self.reset();
		self.blocks.reset_to(hashes);
		self.state = State::Blocks;
	}

	/// Returns used heap memory size.
	pub fn heap_size(&self) -> usize {
		self.blocks.heap_size() + self.round_parents.heap_size_of_children()
	}

	/// Returns best imported block number.
	pub fn last_imported_block_number(&self) -> BlockNumber {
		self.last_imported_block
	}

	/// Add new block headers.
	pub fn import_headers(&mut self, io: &mut SyncIo, r: &Rlp, expected_hash: Option<H256>) -> Result<DownloadAction, BlockDownloaderImportError> {
		let item_count = r.item_count().unwrap_or(0);
		if self.state == State::Idle {
			trace_sync!(self, "Ignored unexpected block headers");
			return Ok(DownloadAction::None)
		}
		if item_count == 0 && (self.state == State::Blocks) {
			return Err(BlockDownloaderImportError::Invalid);
		}

		let mut headers = Vec::new();
		let mut hashes = Vec::new();
		let mut valid_response = item_count == 0; //empty response is valid
		let mut any_known = false;
		for i in 0..item_count {
			let info = SyncHeader::from_rlp(r.at(i)?.as_raw().to_vec())?;
			let number = BlockNumber::from(info.header.number());
			let hash = info.header.hash();
			// Check if any of the headers matches the hash we requested
			if !valid_response {
				if let Some(expected) = expected_hash {
					valid_response = expected == hash;
				}
			}
			any_known = any_known || self.blocks.contains_head(&hash);
			if self.blocks.contains(&hash) {
				trace_sync!(self, "Skipping existing block header {} ({:?})", number, hash);
				continue;
			}

			if self.highest_block.as_ref().map_or(true, |n| number > *n) {
				self.highest_block = Some(number);
			}

			match io.chain().block_status(BlockId::Hash(hash.clone())) {
				BlockStatus::InChain | BlockStatus::Queued => {
					match self.state {
						State::Blocks => trace_sync!(self, "Header already in chain {} ({})", number, hash),
						_ => trace_sync!(self, "Header already in chain {} ({}), state = {:?}", number, hash, self.state),
					}
					headers.push(info);
					hashes.push(hash);
				},
				BlockStatus::Bad => {
					return Err(BlockDownloaderImportError::Invalid);
				},
				BlockStatus::Unknown => {
					headers.push(info);
					hashes.push(hash);
				}
			}
		}

		// Disable the peer for this syncing round if it gives invalid chain
		if !valid_response {
			trace_sync!(self, "Invalid headers response");
			return Err(BlockDownloaderImportError::Invalid);
		}

		match self.state {
			State::ChainHead => {
				if !headers.is_empty() {
					// TODO: validate heads better. E.g. check that there is enough distance between blocks.
					trace_sync!(self, "Received {} subchain heads, proceeding to download", headers.len());
					self.blocks.reset_to(hashes);
					self.state = State::Blocks;
					return Ok(DownloadAction::Reset);
				} else {
					let best = io.chain().chain_info().best_block_number;
					let oldest_reorg = io.chain().pruning_info().earliest_state;
					let last = self.last_imported_block;
					if self.limit_reorg && best > last && (last == 0 || last < oldest_reorg) {
						trace_sync!(self, "No common block, disabling peer");
						return Err(BlockDownloaderImportError::Invalid);
					}
				}
			},
			State::Blocks => {
				let count = headers.len();
				// At least one of the headers must advance the subchain. Otherwise they are all useless.
				if count == 0 || !any_known {
					trace_sync!(self, "No useful headers, expected hash {:?}", expected_hash);
					if let Some(eh) = expected_hash {
						self.useless_headers_count += 1;
						// only reset download if we have multiple subchain heads, to avoid unnecessary resets
						// when we are at the head of the chain when we may legitimately receive no useful headers
						if self.blocks.heads_len() > 1 && self.useless_headers_count >= MAX_USELESS_HEADERS_PER_ROUND {
							trace_sync!(self, "Received consecutive sets of useless headers from requested header {:?}. Resetting sync", eh);
							self.reset();
						}
					}
					return Err(BlockDownloaderImportError::Useless);
				}
				self.blocks.insert_headers(headers);
				trace_sync!(self, "Inserted {} headers", count);
			},
			_ => trace_sync!(self, "Unexpected headers({})", headers.len()),
		}

		Ok(DownloadAction::None)
	}

	/// Called by peer once it has new block bodies
	pub fn import_bodies(&mut self, r: &Rlp) -> Result<(), BlockDownloaderImportError> {
		let item_count = r.item_count().unwrap_or(0);
		if item_count == 0 {
			return Err(BlockDownloaderImportError::Useless);
		} else if self.state != State::Blocks {
			trace_sync!(self, "Ignored unexpected block bodies");
		} else {
			let mut bodies = Vec::with_capacity(item_count);
			for i in 0..item_count {
				let body = SyncBody::from_rlp(r.at(i)?.as_raw())?;
				bodies.push(body);
			}

			if self.blocks.insert_bodies(bodies) != item_count {
				trace_sync!(self, "Deactivating peer for giving invalid block bodies");
				return Err(BlockDownloaderImportError::Invalid);
			}
		}
		Ok(())
	}

	/// Called by peer once it has new block bodies
	pub fn import_receipts(&mut self, _io: &mut SyncIo, r: &Rlp) -> Result<(), BlockDownloaderImportError> {
		let item_count = r.item_count().unwrap_or(0);
		if item_count == 0 {
			return Err(BlockDownloaderImportError::Useless);
		}
		else if self.state != State::Blocks {
			trace_sync!(self, "Ignored unexpected block receipts");
		}
		else {
			let mut receipts = Vec::with_capacity(item_count);
			for i in 0..item_count {
				let receipt = r.at(i).map_err(|e| {
					trace_sync!(self, "Error decoding block receipts RLP: {:?}", e);
					BlockDownloaderImportError::Invalid
				})?;
				receipts.push(receipt.as_raw().to_vec());
			}
			if self.blocks.insert_receipts(receipts) != item_count {
				trace_sync!(self, "Deactivating peer for giving invalid block receipts");
				return Err(BlockDownloaderImportError::Invalid);
			}
		}
		Ok(())
	}

	fn start_sync_round(&mut self, io: &mut SyncIo) {
		self.state = State::ChainHead;
		trace_sync!(self, "Starting round (last imported count = {:?}, last started = {}, block = {:?}", self.imported_this_round, self.last_round_start, self.last_imported_block);
		// Check if need to retract to find the common block. The problem is that the peers still return headers by hash even
		// from the non-canonical part of the tree. So we also retract if nothing has been imported last round.
		let start = self.last_round_start;
		let start_hash = self.last_round_start_hash;
		match self.imported_this_round {
			Some(n) if n == 0 && start > 0 => {
				// nothing was imported last round, step back to a previous block
				// search parent in last round known parents first
				if let Some(&(_, p)) = self.round_parents.iter().find(|&&(h, _)| h == start_hash) {
					self.last_imported_block = start - 1;
					self.last_imported_hash = p.clone();
					trace_sync!(self, "Searching common header from the last round {} ({})", self.last_imported_block, self.last_imported_hash);
				} else {
					let best = io.chain().chain_info().best_block_number;
					let oldest_reorg = io.chain().pruning_info().earliest_state;
					if self.limit_reorg && best > start && start < oldest_reorg {
						debug_sync!(self, "Could not revert to previous ancient block, last: {} ({})", start, start_hash);
						self.reset();
					} else {
						let n = start - cmp::min(self.retract_step, start);
						self.retract_step *= 2;
						match io.chain().block_hash(BlockId::Number(n)) {
							Some(h) => {
								self.last_imported_block = n;
								self.last_imported_hash = h;
								trace_sync!(self, "Searching common header in the blockchain {} ({})", start, self.last_imported_hash);
							}
							None => {
								debug_sync!(self, "Could not revert to previous block, last: {} ({})", start, self.last_imported_hash);
								self.reset();
							}
						}
					}
				}
			},
			_ => {
				self.retract_step = 1;
			},
		}
		self.last_round_start = self.last_imported_block;
		self.last_round_start_hash = self.last_imported_hash;
		self.imported_this_round = None;
	}

	/// Find some headers or blocks to download for a peer.
	pub fn request_blocks(&mut self, io: &mut SyncIo, num_active_peers: usize) -> Option<BlockRequest> {
		match self.state {
			State::Idle => {
				self.start_sync_round(io);
				if self.state == State::ChainHead {
					return self.request_blocks(io, num_active_peers);
				}
			},
			State::ChainHead => {
				if num_active_peers < MAX_PARALLEL_SUBCHAIN_DOWNLOAD {
					// Request subchain headers
					trace_sync!(self, "Starting sync with better chain");
					// Request MAX_HEADERS_TO_REQUEST - 2 headers apart so that
					// MAX_HEADERS_TO_REQUEST would include headers for neighbouring subchains
					return Some(BlockRequest::Headers {
						start: self.last_imported_hash.clone(),
						count: SUBCHAIN_SIZE,
						skip: (MAX_HEADERS_TO_REQUEST - 2) as u64,
					});
				}
			},
			State::Blocks => {
				// check to see if we need to download any block bodies first
				let needed_bodies = self.blocks.needed_bodies(MAX_BODIES_TO_REQUEST, false);
				if !needed_bodies.is_empty() {
					return Some(BlockRequest::Bodies {
						hashes: needed_bodies,
					});
				}

				if self.download_receipts {
					let needed_receipts = self.blocks.needed_receipts(MAX_RECEPITS_TO_REQUEST, false);
					if !needed_receipts.is_empty() {
						return Some(BlockRequest::Receipts {
							hashes: needed_receipts,
						});
					}
				}

				// find subchain to download
				if let Some((h, count)) = self.blocks.needed_headers(MAX_HEADERS_TO_REQUEST, false) {
					return Some(BlockRequest::Headers {
						start: h,
						count: count as u64,
						skip: 0,
					});
				}
			},
			State::Complete => (),
		}
		None
	}

	/// Checks if there are blocks fully downloaded that can be imported into the blockchain and does the import.
	/// Returns DownloadAction::Reset if it is imported all the the blocks it can and all downloading peers should be reset
	pub fn collect_blocks(&mut self, io: &mut SyncIo, allow_out_of_order: bool) -> DownloadAction {
		let mut download_action = DownloadAction::None;
		let mut imported = HashSet::new();
		let blocks = self.blocks.drain();
		let count = blocks.len();
		for block_and_receipts in blocks {
			let block = block_and_receipts.block;
			let receipts = block_and_receipts.receipts;

			let h = block.header.hash();
			let number = block.header.number();
			let parent = *block.header.parent_hash();

			if self.target_hash.as_ref().map_or(false, |t| t == &h) {
				self.state = State::Complete;
				trace_sync!(self, "Sync target reached");
				return download_action;
			}

			let result = if let Some(receipts) = receipts {
				io.chain().queue_ancient_block(block, receipts)
			} else {
				io.chain().import_block(block)
			};

			match result {
				Err(EthcoreError(EthcoreErrorKind::Import(ImportErrorKind::AlreadyInChain), _)) => {
					trace_sync!(self, "Block already in chain {:?}", h);
					self.block_imported(&h, number, &parent);
				},
				Err(EthcoreError(EthcoreErrorKind::Import(ImportErrorKind::AlreadyQueued), _)) => {
					trace_sync!(self, "Block already queued {:?}", h);
					self.block_imported(&h, number, &parent);
				},
				Ok(_) => {
					trace_sync!(self, "Block queued {:?}", h);
					imported.insert(h.clone());
					self.block_imported(&h, number, &parent);
				},
				Err(EthcoreError(EthcoreErrorKind::Block(BlockError::UnknownParent(_)), _)) if allow_out_of_order => {
					break;
				},
				Err(EthcoreError(EthcoreErrorKind::Block(BlockError::UnknownParent(_)), _)) => {
					trace_sync!(self, "Unknown new block parent, restarting sync");
					break;
				},
				Err(EthcoreError(EthcoreErrorKind::Block(BlockError::TemporarilyInvalid(_)), _)) => {
					debug_sync!(self, "Block temporarily invalid: {:?}, restarting sync", h);
					break;
				},
				Err(EthcoreError(EthcoreErrorKind::Queue(QueueErrorKind::Full(limit)), _)) => {
					debug_sync!(self, "Block import queue full ({}), restarting sync", limit);
					download_action = DownloadAction::Reset;
					break;
				},
				Err(e) => {
					debug_sync!(self, "Bad block {:?} : {:?}", h, e);
					download_action = DownloadAction::Reset;
					break;
				}
			}
		}
		trace_sync!(self, "Imported {} of {}", imported.len(), count);
		self.imported_this_round = Some(self.imported_this_round.unwrap_or(0) + imported.len());

		if self.blocks.is_empty() {
			// complete sync round
			trace_sync!(self, "Sync round complete");
			self.reset();
		}
		download_action
	}

	fn block_imported(&mut self, hash: &H256, number: BlockNumber, parent: &H256) {
		self.last_imported_block = number;
		self.last_imported_hash = hash.clone();
		self.round_parents.push_back((hash.clone(), parent.clone()));
		if self.round_parents.len() > MAX_ROUND_PARENTS {
			self.round_parents.pop_front();
		}
	}
}

#[cfg(test)]
mod tests {
	use ethcore::header::*;
	use ethcore::client::{TestBlockChainClient};
	use parking_lot::RwLock;
	use rlp::*;
	use std::collections::{VecDeque};
	use tests::helpers::{TestIo};
	use tests::snapshot::TestSnapshotService;

	use super::*;

	fn get_dummy_headers(count: u32) -> Vec<Header> {
		let mut parent_hash = H256::new();
		let mut tx_root = H256::new();
		let mut headers = Vec::new();
		for i in 0..count {
			let mut header = Header::new();
			header.set_difficulty((i * 100).into());
			header.set_timestamp((i * 10) as u64);
			header.set_number(i as u64);
			header.set_parent_hash(parent_hash);
			header.set_transactions_root(tx_root);
			parent_hash = header.hash();
			tx_root = H256::random();
			headers.push(header);
		}
		headers
	}

	fn import_headers(headers: &[Header], downloader: &mut BlockDownloader, io: &mut SyncIo) -> Result<DownloadAction, BlockDownloaderImportError> {
		let mut stream = RlpStream::new();
		stream.append_list(headers);
		let bytes = stream.out();
		let rlp = Rlp::new(&bytes);
		let expected_hash = headers.first().map(|h| h.hash());
		downloader.import_headers(io, &rlp, expected_hash)
	}

	fn import_headers_ok(headers: &[Header], downloader: &mut BlockDownloader, io: &mut SyncIo) {
		let res = import_headers(headers, downloader, io);
		assert!(res.is_ok());
	}

	#[test]
	fn reset_after_consecutive_sets_of_useless_headers() {
		::env_logger::try_init().ok();
		let headers1 = get_dummy_headers(20);
		let short_subchain = get_dummy_headers(5);

		let mut client = TestBlockChainClient::new();
		let queue = RwLock::new(VecDeque::new());
		let ss = TestSnapshotService::new();
		let mut io = TestIo::new(&mut client, &ss, &queue, None);

		let heads : Vec<_> = headers1.iter()
			.enumerate().filter_map(|(i, h)| if i % 10 == 0 { Some(h.clone()) } else { None }).collect();
		let start_hash = heads[0].hash();

		let mut downloader = BlockDownloader::new(BlockSet::OldBlocks, &start_hash, 0);

		downloader.request_blocks(&mut io, 1);

		import_headers_ok(&heads, &mut downloader, &mut io);
		import_headers_ok(&short_subchain, &mut downloader, &mut io);

		assert_eq!(downloader.state, State::Blocks);
		assert!(!downloader.blocks.is_empty());

		// simulate receiving useless headers
		let head = vec![short_subchain.last().unwrap().clone()];
		let res1 = import_headers(&head, &mut downloader, &mut io);
		let res2 = import_headers(&head, &mut downloader, &mut io);

		assert!(res1.is_err());
		assert!(res2.is_err());
		assert_eq!(downloader.state, State::Idle);
		assert!(downloader.blocks.is_empty());
	}

	#[test]
	fn dont_reset_after_consecutive_sets_of_useless_headers_for_chain_head() {
		::env_logger::try_init().ok();
		let headers = get_dummy_headers(10);
		let short_subchain = get_dummy_headers(5);

		let mut client = TestBlockChainClient::new();
		let queue = RwLock::new(VecDeque::new());
		let ss = TestSnapshotService::new();
		let mut io = TestIo::new(&mut client, &ss, &queue, None);

		let heads : Vec<_> = headers.iter()
			.enumerate().filter_map(|(i, h)| if i % 10 == 0 { Some(h.clone()) } else { None }).collect();
		let start_hash = heads[0].hash();

		let mut downloader = BlockDownloader::new(BlockSet::OldBlocks, &start_hash, 0);

		downloader.request_blocks(&mut io, 1);

		import_headers_ok(&heads, &mut downloader, &mut io);
		import_headers_ok(&short_subchain, &mut downloader, &mut io);

		assert_eq!(downloader.state, State::Blocks);
		assert!(!downloader.blocks.is_empty());

		// simulate receiving useless headers
		let head = vec![short_subchain.last().unwrap().clone()];
		let res1 = import_headers(&head, &mut downloader, &mut io);
		let res2 = import_headers(&head, &mut downloader, &mut io);

		assert!(res1.is_err());
		assert!(res2.is_err());
		// download shouldn't be reset since this is the chain head for a single subchain.
		// this state usually occurs for NewBlocks when it has reached the chain head.
		assert_eq!(downloader.state, State::Blocks);
		assert!(!downloader.blocks.is_empty());
	}
}
